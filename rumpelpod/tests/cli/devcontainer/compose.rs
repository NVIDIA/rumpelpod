// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use indoc::formatdoc;
use rumpelpod::CommandExt;
use rusqlite::Connection;

use crate::common::{pod_command, TestDaemon, TestHome, TestRepo, TEST_REPO_PATH, TEST_USER};
use crate::executor::{ExecutorMode, ExecutorResources};

fn compose_available() -> bool {
    Command::new("docker")
        .args(["compose", "version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn require_local_compose() -> bool {
    match crate::executor::executor_mode() {
        ExecutorMode::Docker => {
            assert!(
                compose_available(),
                "Docker-mode integration tests require the Docker Compose plugin"
            );
            true
        }
        ExecutorMode::Podman | ExecutorMode::Ssh | ExecutorMode::K8s => {
            crate::executor::skip_test();
            false
        }
    }
}

fn write_compose_devcontainer(
    repo: &TestRepo,
    agent_command: &str,
    agent_config: &str,
    extra_services: &str,
    extra_devcontainer: &str,
) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("create .devcontainer");
    fs::write(
        devcontainer_dir.join("Dockerfile"),
        formatdoc! {r#"
            FROM cgr.dev/chainguard/wolfi-base
            RUN apk add --no-cache git shadow socat
            RUN useradd -m -u 1000 {TEST_USER}
            RUN mkdir -p /home/{TEST_USER}/data && chown {TEST_USER} /home/{TEST_USER}/data
            USER root
        "#},
    )
    .expect("write compose Dockerfile");
    let compose = format!(
        "services:\n  agent:\n    build:\n      context: ..\n      dockerfile: .devcontainer/Dockerfile\n    command: {agent_command}\n{agent_config}\n{extra_services}\n"
    );
    fs::write(devcontainer_dir.join("compose.yaml"), compose).expect("write compose file");
    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        formatdoc! {r#"
            {{
                "dockerComposeFile": "compose.yaml",
                "service": "agent",
                "workspaceFolder": "{TEST_REPO_PATH}",
                "containerUser": "{TEST_USER}"{extra_devcontainer}
            }}
        "#},
    )
    .expect("write compose devcontainer.json");
}

fn try_echo(port: u16, message: &str) -> Option<String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream.write_all(message.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

fn forwarded_local_port(repo: &TestRepo, daemon: &TestDaemon, pod_name: &str, target: &str) -> u16 {
    let output = pod_command(repo, daemon)
        .args(["ports", pod_name])
        .success()
        .expect("list compose ports");
    let output = String::from_utf8_lossy(&output);
    output
        .lines()
        .find_map(|line| {
            let mut columns = line.split_whitespace();
            (columns.next() == Some(target))
                .then(|| columns.next().expect("forwarded port has a local port"))
        })
        .expect("forwarded target is listed")
        .parse()
        .expect("local port is numeric")
}

fn compose_agent_container(repo: &TestRepo, pod_name: &str) -> String {
    let pod_filter = format!("label=dev.rumpelpod.name={pod_name}");
    let repo_filter = format!("label=dev.rumpelpod.repo_path={}", repo.path().display());
    let output = Command::new("docker")
        .args([
            "container",
            "ls",
            "--all",
            "--quiet",
            "--filter",
            &pod_filter,
            "--filter",
            &repo_filter,
        ])
        .output()
        .expect("query compose agent container");
    assert!(
        output.status.success(),
        "docker container ls failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let containers: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    let [container] = containers.as_slice() else {
        panic!("expected one compose agent container, got {containers:?}");
    };
    (*container).to_string()
}

fn container_label(container: &str, label: &str) -> String {
    let format = format!("{{{{ index .Config.Labels {label:?} }}}}");
    let output = Command::new("docker")
        .args(["container", "inspect", "--format", &format, container])
        .output()
        .expect("inspect compose container label");
    assert!(
        output.status.success(),
        "docker container inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn compose_service_container(project: &str, service: &str) -> String {
    let project_filter = format!("label=com.docker.compose.project={project}");
    let service_filter = format!("label=com.docker.compose.service={service}");
    let output = Command::new("docker")
        .args([
            "container",
            "ls",
            "--all",
            "--quiet",
            "--filter",
            &project_filter,
            "--filter",
            &service_filter,
        ])
        .output()
        .expect("query compose service container");
    assert!(
        output.status.success(),
        "docker container ls failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let containers: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    let [container] = containers.as_slice() else {
        panic!("expected one container for compose service '{service}', got {containers:?}");
    };
    (*container).to_string()
}

#[test]
fn compose_build_cache_reuses_images_and_tracks_context_content() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(&repo, r#"["sleep", "infinity"]"#, "", "", "");
    let marker = repo.path().join(".devcontainer/cache-marker.txt");
    fs::write(&marker, "version-1\n").expect("write compose cache marker");
    let dockerfile = repo.path().join(".devcontainer/Dockerfile");
    let mut contents = fs::read_to_string(&dockerfile).expect("read compose Dockerfile");
    contents.push_str(
        "COPY .devcontainer/cache-marker.txt /cache-marker.txt\nRUN echo compose-cache-build-marker > /build-marker\n",
    );
    fs::write(&dockerfile, contents).expect("extend compose Dockerfile");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let first = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "compose-cache-1",
            "--",
            "cat",
            "/cache-marker.txt",
        ])
        .output()
        .expect("create first compose cache pod");
    assert!(first.status.success());
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert_eq!(first_stdout.lines().last(), Some("version-1"));
    let first_output = format!("{first_stdout}{first_stderr}");
    assert!(
        first_output.contains("compose-cache-build-marker"),
        "first pod should run the Compose build: {first_output}"
    );

    let second = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "compose-cache-2",
            "--",
            "cat",
            "/cache-marker.txt",
        ])
        .output()
        .expect("create second compose cache pod");
    assert!(second.status.success());
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert_eq!(second_stdout.lines().last(), Some("version-1"));
    let second_output = format!("{second_stdout}{second_stderr}");
    assert!(
        !second_output.contains("compose-cache-build-marker"),
        "second pod should reuse the Compose image: {second_output}"
    );

    fs::write(&marker, "version-2\n").expect("update compose cache marker");
    let third = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "compose-cache-3",
            "--",
            "cat",
            "/cache-marker.txt",
        ])
        .output()
        .expect("create third compose cache pod");
    assert!(third.status.success());
    let third_stdout = String::from_utf8_lossy(&third.stdout);
    let third_stderr = String::from_utf8_lossy(&third.stderr);
    assert_eq!(third_stdout.lines().last(), Some("version-2"));
    let third_output = format!("{third_stdout}{third_stderr}");
    assert!(
        third_output.contains("compose-cache-build-marker"),
        "changed context should rerun the Compose build: {third_output}"
    );

    for pod in ["compose-cache-1", "compose-cache-2", "compose-cache-3"] {
        pod_command(&repo, &daemon)
            .args(["delete", "--force", "--wait", pod])
            .success()
            .expect("delete compose cache project");
    }
}

#[test]
fn compose_environment_secret_uses_client_command_environment() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(
        &repo,
        r#"["sleep", "infinity"]"#,
        r#"    secrets:
      - api_key"#,
        r#"secrets:
  api_key:
    environment: RUMPELPOD_TEST_COMPOSE_SECRET"#,
        "",
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .env("RUMPELPOD_TEST_COMPOSE_SECRET", "client-only-secret")
        .args([
            "enter",
            "--create",
            "compose-secret",
            "--",
            "cat",
            "/run/secrets/api_key",
        ])
        .success()
        .expect("create Compose pod with an environment-backed secret");
    assert_eq!(
        String::from_utf8_lossy(&output).lines().last(),
        Some("client-only-secret")
    );

    let database = home.path().join("state/rumpelpod/db.sqlite");
    let connection = Connection::open(database).expect("open pod database");
    let stored: (String, String, String) = connection
        .query_row(
            "SELECT devcontainer_json, compose_config, local_env FROM pods WHERE name = 'compose-secret'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read stored Compose pod configuration");
    assert!(
        !stored.0.contains("client-only-secret")
            && !stored.1.contains("client-only-secret")
            && !stored.2.contains("client-only-secret"),
        "client environment secret was persisted in the pod database"
    );
    drop(connection);

    pod_command(&repo, &daemon)
        .args(["delete", "--force", "--wait", "compose-secret"])
        .success()
        .expect("delete Compose secret project");
}

#[test]
fn compose_no_cache_builds_every_project() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(&repo, r#"["sleep", "infinity"]"#, "", "", "");
    let dockerfile = repo.path().join(".devcontainer/Dockerfile");
    let mut dockerfile_contents = fs::read_to_string(&dockerfile).expect("read Dockerfile");
    dockerfile_contents.push_str("RUN echo compose-no-cache-marker > /no-cache-marker\n");
    fs::write(&dockerfile, dockerfile_contents).expect("extend Dockerfile");
    let compose_path = repo.path().join(".devcontainer/compose.yaml");
    let compose = fs::read_to_string(&compose_path).expect("read compose file");
    let compose = compose.replace(
        "      dockerfile: .devcontainer/Dockerfile\n",
        "      dockerfile: .devcontainer/Dockerfile\n      no_cache: true\n",
    );
    fs::write(&compose_path, compose).expect("enable no-cache Compose builds");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    for pod in ["compose-no-cache-1", "compose-no-cache-2"] {
        let output = pod_command(&repo, &daemon)
            .args(["enter", "--create", pod, "--", "true"])
            .output()
            .expect("create no-cache Compose pod");
        assert!(output.status.success());
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            combined.contains("compose-no-cache-marker"),
            "no_cache should invoke the Compose build for {pod}: {combined}"
        );
    }

    for pod in ["compose-no-cache-1", "compose-no-cache-2"] {
        pod_command(&repo, &daemon)
            .args(["delete", "--force", "--wait", pod])
            .success()
            .expect("delete no-cache Compose project");
    }
}

#[test]
fn compose_sidecar_forward_survives_project_restart() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(
        &repo,
        r#"["sleep", "infinity"]"#,
        "",
        r#"  db:
    build:
      context: ..
      dockerfile: .devcontainer/Dockerfile
    command: ["socat", "TCP-LISTEN:19876,fork,reuseaddr", "EXEC:cat"]"#,
        r#",
                "forwardPorts": ["db:19876"]"#,
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let mut daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "compose-forward", "--", "true"])
        .success()
        .expect("create compose pod");

    let local_port = forwarded_local_port(&repo, &daemon, "compose-forward", "db:19876");
    assert_eq!(
        try_echo(local_port, "compose sidecar"),
        Some("compose sidecar".into())
    );

    daemon.kill();
    drop(daemon);
    let daemon = TestDaemon::start(&home);
    let local_port = forwarded_local_port(&repo, &daemon, "compose-forward", "db:19876");
    assert_eq!(
        try_echo(local_port, "after daemon restart"),
        Some("after daemon restart".into())
    );

    let agent = compose_agent_container(&repo, "compose-forward");
    let project = container_label(&agent, "com.docker.compose.project");
    let old_sidecar = compose_service_container(&project, "db");
    Command::new("docker")
        .args(["compose", "--project-name", &project, "--file"])
        .arg(repo.path().join(".devcontainer/compose.yaml"))
        .args(["up", "--detach", "--no-build", "--force-recreate", "db"])
        .success()
        .expect("force-recreate compose sidecar");
    let new_sidecar = compose_service_container(&project, "db");
    assert_ne!(old_sidecar, new_sidecar);

    pod_command(&repo, &daemon)
        .args(["connect", "compose-forward"])
        .success()
        .expect("repair compose connection after sidecar recreation");
    let local_port = forwarded_local_port(&repo, &daemon, "compose-forward", "db:19876");
    assert_eq!(
        try_echo(local_port, "after sidecar recreation"),
        Some("after sidecar recreation".into())
    );

    pod_command(&repo, &daemon)
        .args(["stop", "--wait", "compose-forward"])
        .success()
        .expect("stop compose project");
    pod_command(&repo, &daemon)
        .args(["enter", "compose-forward", "--", "true"])
        .success()
        .expect("restart compose project");
    let local_port = forwarded_local_port(&repo, &daemon, "compose-forward", "db:19876");
    assert_eq!(
        try_echo(local_port, "after restart"),
        Some("after restart".into())
    );

    pod_command(&repo, &daemon)
        .args(["delete", "--force", "--wait", "compose-forward"])
        .success()
        .expect("delete compose project");
}

#[test]
fn compose_reenter_recreates_project_after_agent_removal() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(
        &repo,
        r#"["sleep", "infinity"]"#,
        "",
        r#"  db:
    build:
      context: ..
      dockerfile: .devcontainer/Dockerfile
    command: ["sleep", "infinity"]"#,
        "",
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "compose-recover", "--", "true"])
        .success()
        .expect("create compose recovery project");
    let old_agent = compose_agent_container(&repo, "compose-recover");
    let project = container_label(&old_agent, "com.docker.compose.project");
    let old_sidecar = compose_service_container(&project, "db");
    Command::new("docker")
        .args(["container", "rm", "--force", &old_agent])
        .success()
        .expect("remove compose agent externally");

    pod_command(&repo, &daemon)
        .args(["enter", "compose-recover", "--", "true"])
        .success()
        .expect("re-enter should recreate the missing compose agent");
    let new_agent = compose_agent_container(&repo, "compose-recover");
    let new_sidecar = compose_service_container(&project, "db");
    assert_ne!(old_agent, new_agent);
    assert_ne!(old_sidecar, new_sidecar);
    let old_sidecar_status = Command::new("docker")
        .args(["container", "inspect", &old_sidecar])
        .status()
        .expect("inspect old compose sidecar");
    assert!(!old_sidecar_status.success());

    pod_command(&repo, &daemon)
        .args(["delete", "--force", "--wait", "compose-recover"])
        .success()
        .expect("delete recovered compose project");
}

#[test]
fn compose_runtime_sidecar_forward_persists() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(
        &repo,
        r#"["sleep", "infinity"]"#,
        "",
        r#"  db:
    build:
      context: ..
      dockerfile: .devcontainer/Dockerfile
    command: ["socat", "TCP-LISTEN:19877,fork,reuseaddr", "EXEC:cat"]"#,
        "",
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let mut daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "compose-runtime", "--", "true"])
        .success()
        .expect("create compose runtime-forward project");
    let unknown = pod_command(&repo, &daemon)
        .args([
            "forward-port",
            "--service",
            "unknown",
            "compose-runtime:19877",
        ])
        .output()
        .expect("try unknown compose forwarding service");
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("does not exist"));

    pod_command(&repo, &daemon)
        .args(["forward-port", "--service", "db", "compose-runtime:19877"])
        .success()
        .expect("add runtime sidecar forward");
    let local_port = forwarded_local_port(&repo, &daemon, "compose-runtime", "db:19877");
    assert_eq!(
        try_echo(local_port, "runtime compose forward"),
        Some("runtime compose forward".into())
    );

    daemon.kill();
    drop(daemon);
    let daemon = TestDaemon::start(&home);
    let local_port = forwarded_local_port(&repo, &daemon, "compose-runtime", "db:19877");
    assert_eq!(
        try_echo(local_port, "restored runtime forward"),
        Some("restored runtime forward".into())
    );

    pod_command(&repo, &daemon)
        .args(["delete", "--force", "--wait", "compose-runtime"])
        .success()
        .expect("delete runtime-forward Compose project");
}

#[test]
fn compose_remote_docker_launches_sidecars_and_rejects_bind_mounts() {
    if !matches!(crate::executor::executor_mode(), ExecutorMode::Docker) {
        crate::executor::skip_test();
        return;
    }
    assert!(
        compose_available(),
        "Docker-mode integration tests require the Docker Compose plugin"
    );

    let repo = TestRepo::new();
    write_compose_devcontainer(
        &repo,
        r#"["sleep", "infinity"]"#,
        r#"    volumes:
      - compose-data:/tmp/compose-data
    secrets:
      - api_key"#,
        r#"  db:
    build:
      context: ..
      dockerfile: .devcontainer/Dockerfile
    command: ["socat", "TCP-LISTEN:19881,fork,reuseaddr", "EXEC:cat"]
volumes:
  compose-data:
secrets:
  api_key:
    environment: RUMPELPOD_TEST_REMOTE_COMPOSE_SECRET"#,
        r#",
                "forwardPorts": ["db:19881"]"#,
    );
    let bind_repo = TestRepo::new();
    write_compose_devcontainer(
        &bind_repo,
        r#"["sleep", "infinity"]"#,
        r#"    volumes:
      - ../bind-source:/tmp/bind-source"#,
        "",
        "",
    );
    fs::create_dir(bind_repo.path().join("bind-source")).expect("create bind source");

    let home = TestHome::new();
    let executor = ExecutorResources::ssh(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    fs::write(bind_repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .env(
            "RUMPELPOD_TEST_REMOTE_COMPOSE_SECRET",
            "remote-client-only-secret",
        )
        .args([
            "enter",
            "--create",
            "compose-remote",
            "--",
            "cat",
            "/run/secrets/api_key",
        ])
        .success()
        .expect("create remote Compose project");
    assert_eq!(
        String::from_utf8_lossy(&output).lines().last(),
        Some("remote-client-only-secret")
    );
    let local_port = forwarded_local_port(&repo, &daemon, "compose-remote", "db:19881");
    assert_eq!(
        try_echo(local_port, "remote sidecar forward"),
        Some("remote sidecar forward".into())
    );

    let output = pod_command(&bind_repo, &daemon)
        .args(["enter", "--create", "compose-remote-bind", "--", "true"])
        .output()
        .expect("try remote Compose bind mount");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not supported with a remote Docker host"),
        "unexpected remote bind error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    pod_command(&repo, &daemon)
        .args(["delete", "--force", "--wait", "compose-remote"])
        .success()
        .expect("delete remote Compose project");
}

#[test]
fn compose_build_forwards_client_ssh_agent() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    std::env::remove_var("SSH_AUTH_SOCK");
    write_compose_devcontainer(&repo, r#"["sleep", "infinity"]"#, "", "", "");
    let unique = repo.path().file_name().unwrap().to_string_lossy();
    let dockerfile = repo.path().join(".devcontainer/Dockerfile");
    let mut dockerfile_contents = fs::read_to_string(&dockerfile).expect("read Dockerfile");
    dockerfile_contents.push_str(&format!(
        "RUN --mount=type=ssh test -S \"$SSH_AUTH_SOCK\" && echo {unique:?} > /tmp/compose-ssh-agent\n"
    ));
    fs::write(&dockerfile, dockerfile_contents).expect("extend Compose Dockerfile");
    let compose_path = repo.path().join(".devcontainer/compose.yaml");
    let compose = fs::read_to_string(&compose_path).expect("read compose file");
    let compose = compose.replace(
        "      dockerfile: .devcontainer/Dockerfile\n",
        "      dockerfile: .devcontainer/Dockerfile\n      ssh:\n        - default\n",
    );
    fs::write(&compose_path, compose).expect("enable Compose SSH forwarding");

    let home = TestHome::new();
    home.link_local_bin("ssh-agent");
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let agent_sock = home.path().join("compose-client-agent.sock");
    let agent_out = Command::new("ssh-agent")
        .args(["-a"])
        .arg(&agent_sock)
        .output()
        .expect("ssh-agent failed");
    assert!(agent_out.status.success());
    let agent_stdout = String::from_utf8_lossy(&agent_out.stdout);
    let agent_pid: u32 = agent_stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("SSH_AGENT_PID=")
                .and_then(|value| value.split(';').next())
        })
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("parsing agent PID from: {agent_stdout}"));

    let output = pod_command(&repo, &daemon)
        .env("SSH_AUTH_SOCK", &agent_sock)
        .args([
            "enter",
            "--create",
            "compose-ssh-build",
            "--",
            "cat",
            "/tmp/compose-ssh-agent",
        ])
        .output()
        .expect("create Compose pod with SSH build mount");

    let _ = Command::new("ssh-agent")
        .arg("-k")
        .env("SSH_AUTH_SOCK", &agent_sock)
        .env("SSH_AGENT_PID", agent_pid.to_string())
        .output();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Compose SSH build failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains(unique.as_ref()),
        "expected build marker {unique} in {stdout}"
    );

    pod_command(&repo, &daemon)
        .args(["delete", "--force", "--wait", "compose-ssh-build"])
        .success()
        .expect("delete Compose SSH-build project");
}

#[test]
fn compose_override_command_and_run_services_are_applied() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(
        &repo,
        r#"["false"]"#,
        r#"    links:
      - db"#,
        r#"  db:
    build:
      context: ..
      dockerfile: .devcontainer/Dockerfile
    command: ["socat", "TCP-LISTEN:19878,fork,reuseaddr", "EXEC:cat"]
  unused:
    build:
      context: ..
      dockerfile: .devcontainer/Dockerfile
    command: ["sleep", "infinity"]"#,
        r#",
                "overrideCommand": true,
                "runServices": ["agent"],
                "forwardPorts": ["db:19878"]"#,
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "compose-override", "--", "true"])
        .success()
        .expect("overrideCommand should replace the exiting compose command");
    let local_port = forwarded_local_port(&repo, &daemon, "compose-override", "db:19878");
    assert_eq!(
        try_echo(local_port, "dependency forward"),
        Some("dependency forward".into())
    );

    let repo_filter = format!("label=dev.rumpelpod.repo_path={}", repo.path().display());
    let unused = Command::new("docker")
        .args([
            "container",
            "ls",
            "--all",
            "--quiet",
            "--filter",
            "label=com.docker.compose.service=unused",
        ])
        .args(["--filter", &repo_filter])
        .output()
        .expect("query unused compose service");
    assert!(
        unused.status.success(),
        "docker container ls failed: {}",
        String::from_utf8_lossy(&unused.stderr)
    );
    assert!(
        unused.stdout.is_empty(),
        "runServices created the unselected service"
    );
}

#[test]
fn compose_default_command_exit_has_actionable_error() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(&repo, r#"["false"]"#, "", "", "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "compose-exits", "--", "true"])
        .output()
        .expect("run compose pod with exiting command");
    assert!(
        !output.status.success(),
        "exiting agent service should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exited") && stderr.contains("overrideCommand"),
        "compose exit error should explain the remedy: {stderr}"
    );
}

#[test]
fn compose_service_user_is_used_when_devcontainer_has_no_user() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(
        &repo,
        r#"["sleep", "infinity"]"#,
        &format!("    user: {TEST_USER}"),
        "",
        "",
    );
    let devcontainer_path = repo.path().join(".devcontainer/devcontainer.json");
    let devcontainer = fs::read_to_string(&devcontainer_path).unwrap();
    let user_property = format!(",\n    \"containerUser\": \"{TEST_USER}\"");
    let devcontainer_without_user = devcontainer.replace(&user_property, "");
    assert_ne!(devcontainer, devcontainer_without_user);
    fs::write(&devcontainer_path, devcontainer_without_user).unwrap();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "compose-user", "--", "id", "-un"])
        .success()
        .expect("create compose pod with a service-level user");
    assert_eq!(
        String::from_utf8_lossy(&output).lines().last(),
        Some(TEST_USER)
    );

    pod_command(&repo, &daemon)
        .args(["delete", "--force", "--wait", "compose-user"])
        .success()
        .expect("delete compose project");
}

#[test]
fn compose_fork_gets_independent_project_resources() {
    if !require_local_compose() {
        return;
    }

    let repo = TestRepo::new();
    write_compose_devcontainer(
        &repo,
        r#"["sleep", "infinity"]"#,
        r#"    container_name: fixed-compose-container
    volumes:
      - data:/home/testuser/data"#,
        r#"volumes:
  data:"#,
        "",
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "compose-source",
            "--",
            "sh",
            "-c",
            "mkdir -p /home/testuser/data && echo source > /home/testuser/data/marker",
        ])
        .success()
        .expect("create source compose pod");
    pod_command(&repo, &daemon)
        .args(["fork", "compose-source", "compose-fork"])
        .success()
        .expect("fork compose pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "compose-fork",
            "--",
            "test",
            "!",
            "-e",
            "/home/testuser/data/marker",
        ])
        .success()
        .expect("fork should not share the source project's named volume");

    for pod in ["compose-source", "compose-fork"] {
        pod_command(&repo, &daemon)
            .args(["delete", "--force", "--wait", pod])
            .success()
            .expect("delete compose project");
    }
}
