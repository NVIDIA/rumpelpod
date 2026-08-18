// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use indoc::formatdoc;
use rumpelpod::CommandExt;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::{self, ExecutorResources};

fn container_id(pod_name: &str) -> String {
    let output = Command::new("docker")
        .args([
            "ps",
            "-q",
            "--filter",
            &format!("label=dev.rumpelpod.name={pod_name}"),
        ])
        .success()
        .expect("docker ps failed");
    let id = String::from_utf8_lossy(&output).trim().to_string();
    assert!(!id.is_empty(), "container for pod '{pod_name}' not found");
    id
}

fn process_pid(container_id: &str, pattern: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["exec", container_id, "pgrep", "-f", pattern])
        .output()
        .expect("docker exec pgrep failed");
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::to_string)
}

fn tunnel_server_pid(container_id: &str) -> Option<String> {
    process_pid(container_id, "/opt/rumpelpod/bin/rumpel tunnel-server")
}

fn container_server_pid(container_id: &str) -> Option<String> {
    process_pid(container_id, "/opt/rumpelpod/bin/rumpel container-serve")
}

fn wait_for_new_tunnel_server(container_id: &str, old_pid: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(pid) = tunnel_server_pid(container_id) {
            if pid != old_pid {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "tunnel server in container '{container_id}' was not replaced"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop_container_server(container_id: &str) {
    let pid = container_server_pid(container_id).expect("container server not running");
    Command::new("docker")
        .args(["exec", container_id, "kill", &pid])
        .success()
        .expect("stopping container server failed");

    let deadline = Instant::now() + Duration::from_secs(10);
    while container_server_pid(container_id).as_deref() == Some(pid.as_str()) {
        assert!(
            Instant::now() < deadline,
            "container server in '{container_id}' did not stop"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn connect_preserves_healthy_transport_and_repairs_a_failed_pod() {
    if !matches!(executor::executor_mode(), executor::ExecutorMode::Docker) {
        executor::skip_test();
        return;
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "RUN apk add --no-cache procps", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    for name in ["connect-a", "connect-b"] {
        pod_command(&repo, &daemon)
            .args(["enter", "--create", name, "--", "true"])
            .success()
            .unwrap_or_else(|e| panic!("creating pod '{name}' failed: {e}"));
    }

    let container_a = container_id("connect-a");
    let container_b = container_id("connect-b");
    let mut tunnel_a = tunnel_server_pid(&container_a).expect("pod A tunnel server not running");
    let tunnel_b = tunnel_server_pid(&container_b).expect("pod B tunnel server not running");
    let container_server_a =
        container_server_pid(&container_a).expect("pod A container server not running");

    pod_command(&repo, &daemon)
        .args(["connect", "connect-a"])
        .success()
        .expect("probing healthy connections failed");
    assert_eq!(
        tunnel_server_pid(&container_a).as_deref(),
        Some(tunnel_a.as_str()),
        "connect replaced the target pod's healthy tunnel"
    );
    assert_eq!(
        tunnel_server_pid(&container_b).as_deref(),
        Some(tunnel_b.as_str()),
        "connect replaced a sibling pod's healthy tunnel"
    );

    Command::new("docker")
        .args(["exec", &container_a, "kill", &tunnel_a])
        .success()
        .expect("stopping tunnel server failed");
    pod_command(&repo, &daemon)
        .args(["connect", "connect-a"])
        .success()
        .expect("repairing a failed git connection failed");
    tunnel_a = wait_for_new_tunnel_server(&container_a, &tunnel_a);
    assert_eq!(
        container_server_pid(&container_a).as_deref(),
        Some(container_server_a.as_str()),
        "repairing the git connection replaced a healthy pod server"
    );

    stop_container_server(&container_a);
    pod_command(&repo, &daemon)
        .args(["connect", "connect-a"])
        .success()
        .expect("repairing a failed pod connection failed");
    assert_eq!(
        tunnel_server_pid(&container_a).as_deref(),
        Some(tunnel_a.as_str()),
        "repairing the pod server replaced a healthy git connection"
    );
    assert_eq!(
        tunnel_server_pid(&container_b).as_deref(),
        Some(tunnel_b.as_str()),
        "repairing one pod replaced a sibling pod's healthy tunnel"
    );
}

#[test]
fn connect_rejects_an_unknown_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let daemon = TestDaemon::start(&home);

    let output = pod_command(&repo, &daemon)
        .args(["connect", "missing"])
        .output()
        .expect("running connect failed");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("pod 'missing' not found"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn connect_does_not_start_a_stopped_pod() {
    if !matches!(executor::executor_mode(), executor::ExecutorMode::Docker) {
        executor::skip_test();
        return;
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "connect-stopped", "--", "true"])
        .success()
        .expect("creating pod failed");
    let container = container_id("connect-stopped");
    pod_command(&repo, &daemon)
        .args(["stop", "--wait", "connect-stopped"])
        .success()
        .expect("stopping pod failed");

    let output = pod_command(&repo, &daemon)
        .args(["connect", "connect-stopped"])
        .output()
        .expect("running connect failed");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("pod 'connect-stopped' is stopped"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new("docker")
        .args(["inspect", "--format", "{{.State.Running}}", &container])
        .success()
        .expect("inspecting stopped pod failed");
    assert_eq!(String::from_utf8_lossy(&output).trim(), "false");
}

#[test]
fn connect_rejects_and_recreate_waits_for_stop_in_progress() {
    if !matches!(executor::executor_mode(), executor::ExecutorMode::Docker) {
        executor::skip_test();
        return;
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let docker_path = fs::canonicalize(home.bin_dir().join("docker"))
        .expect("resolving the real docker binary failed");
    fs::remove_file(home.bin_dir().join("docker")).expect("removing docker symlink failed");
    let stop_started = home.path().join("stop-started");
    let release_stop = home.path().join("release-stop");
    let wrapper = formatdoc! {r#"
        #!/bin/sh
        for arg in "$@"; do
            if [ "$arg" = "stop" ]; then
                touch "{stop_started}"
                while [ ! -e "{release_stop}" ]; do
                    sleep 0.1
                done
                break
            fi
        done
        exec "{docker_path}" "$@"
        "#,
        stop_started = stop_started.display(),
        release_stop = release_stop.display(),
        docker_path = docker_path.display(),
    };
    let wrapper_path = home.bin_dir().join("docker");
    fs::write(&wrapper_path, wrapper).expect("writing docker wrapper failed");
    let mut permissions = fs::metadata(&wrapper_path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper_path, permissions)
        .expect("making docker wrapper executable failed");

    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "connect-stopping", "--", "true"])
        .success()
        .expect("creating pod failed");
    let container = container_id("connect-stopping");

    pod_command(&repo, &daemon)
        .args(["stop", "connect-stopping"])
        .success()
        .expect("starting background stop failed");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stop_started.exists() {
        assert!(
            Instant::now() < deadline,
            "docker stop did not reach the test barrier"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let output = pod_command(&repo, &daemon)
        .args(["connect", "connect-stopping"])
        .output()
        .expect("running connect failed");

    let mut recreate_command = pod_command(&repo, &daemon);
    recreate_command
        .args(["recreate", "connect-stopping"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut recreate = recreate_command
        .spawn()
        .expect("starting concurrent recreate failed");
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        recreate
            .try_wait()
            .expect("checking recreate status failed")
            .is_none(),
        "recreate finished before the background stop was released"
    );

    fs::write(&release_stop, "").expect("releasing docker stop failed");
    let recreate_output = recreate
        .wait_with_output()
        .expect("waiting for concurrent recreate failed");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("pod 'connect-stopping' is stopping"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        recreate_output.status.success(),
        "recreate failed after stop completed: {}",
        String::from_utf8_lossy(&recreate_output.stderr)
    );

    let replacement = container_id("connect-stopping");
    assert_ne!(
        replacement, container,
        "recreate did not replace the container"
    );
    let inspect = Command::new(&docker_path)
        .args(["inspect", "--format", "{{.State.Running}}", &replacement])
        .success()
        .expect("inspecting replacement pod failed");
    assert_eq!(String::from_utf8_lossy(&inspect).trim(), "true");
}
