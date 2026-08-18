// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for devcontainer.json port forwarding.
//!
//! Tests verify:
//! - Basic port forwarding from container to host
//! - Multiple ports forwarded simultaneously
//! - Port label display from portsAttributes
//! - Multi-pod port conflict resolution (remapping)
//! - `rumpel ports` CLI command output

use indoc::formatdoc;
use rumpelpod::daemon::protocol::{Daemon, DaemonClient, OnAutoForward, PodName, PortProtocol};
use rumpelpod::CommandExt;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::common::{
    pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo, TEST_REPO_PATH, TEST_USER,
};
use crate::executor::{self, ExecutorResources};

/// Write a devcontainer.json with port forwarding configuration.
fn write_devcontainer_with_ports(repo: &TestRepo, ports_config: &str) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("Failed to create .devcontainer directory");

    // Use socat in the image so we can create simple TCP listeners for testing
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git shadow socat procps
        RUN useradd -m -u 1000 {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
    "#};
    fs::write(devcontainer_dir.join("Dockerfile"), dockerfile).expect("Failed to write Dockerfile");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": ".."
            }},
            {ports_config}
            "workspaceFolder": "{TEST_REPO_PATH}",
            "containerUser": "{TEST_USER}"
        }}
    "#};

    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("Failed to write devcontainer.json");
}

/// Start a TCP echo server inside the container on the given port.
/// Uses socat to listen and echo back whatever is sent.
fn start_echo_server_in_container(repo: &TestRepo, daemon: &TestDaemon, pod_name: &str, port: u16) {
    pod_command(repo, daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            &format!("nohup socat TCP-LISTEN:{port},fork,reuseaddr EXEC:'cat' >/dev/null 2>&1 &"),
        ])
        .success()
        .expect("Failed to start echo server");

    // Give socat a moment to start listening
    std::thread::sleep(Duration::from_millis(500));
}

/// Try to connect to a local port and send/receive data.
fn try_echo(port: u16, message: &str) -> Option<String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream.write_all(message.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    Some(buf)
}

fn stop_container_server(pod_name: &str) {
    let output = Command::new("docker")
        .args([
            "ps",
            "-q",
            "--filter",
            &format!("label=dev.rumpelpod.name={pod_name}"),
        ])
        .success()
        .expect("docker ps failed");
    let container_id = String::from_utf8_lossy(&output).trim().to_string();
    assert!(!container_id.is_empty(), "container not found");

    let find_pid = || -> Option<String> {
        let output = Command::new("docker")
            .args([
                "exec",
                &container_id,
                "pgrep",
                "-f",
                "/opt/rumpelpod/bin/rumpel container-serve",
            ])
            .output()
            .expect("docker exec pgrep failed");
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .map(str::to_string)
    };

    let pid = find_pid().expect("container server not running");
    Command::new("docker")
        .args(["exec", &container_id, "kill", &pid])
        .success()
        .expect("stopping container server failed");

    let deadline = Instant::now() + Duration::from_secs(10);
    while find_pid().as_deref() == Some(pid.as_str()) {
        assert!(Instant::now() < deadline, "container server did not stop");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn forward_single_port() {
    let repo = TestRepo::new();

    write_devcontainer_with_ports(&repo, r#""forwardPorts": [9100],"#);
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create pod (this should set up port forwarding)
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-single", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    // Start an echo server on port 9100 inside the container
    start_echo_server_in_container(&repo, &daemon, "fwd-single", 9100);

    // Verify we can reach it through the forwarded port
    let response = try_echo(9100, "hello port forwarding");
    assert_eq!(
        response.as_deref(),
        Some("hello port forwarding"),
        "Expected echo response through forwarded port 9100"
    );
}

#[test]
fn string_forward_port_requires_compose_service() {
    let repo = TestRepo::new();

    write_devcontainer_with_ports(&repo, r#""forwardPorts": ["9101"],"#);
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-invalid-string", "--", "true"])
        .output()
        .expect("run pod with invalid string forward");
    assert!(
        !output.status.success(),
        "invalid forwardPorts entry succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires a dockerComposeFile pod") && stderr.contains("<service>:<port>"),
        "invalid forwardPorts error was not actionable: {stderr}"
    );
}

#[test]
fn forward_multiple_ports() {
    let repo = TestRepo::new();

    write_devcontainer_with_ports(&repo, r#""forwardPorts": [9200, 9201],"#);
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-multi", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    start_echo_server_in_container(&repo, &daemon, "fwd-multi", 9200);
    start_echo_server_in_container(&repo, &daemon, "fwd-multi", 9201);

    let r1 = try_echo(9200, "port-9200");
    let r2 = try_echo(9201, "port-9201");

    assert_eq!(r1.as_deref(), Some("port-9200"));
    assert_eq!(r2.as_deref(), Some("port-9201"));
}

#[test]
fn ports_command_shows_forwarded_ports() {
    let repo = TestRepo::new();

    write_devcontainer_with_ports(
        &repo,
        r#"
        "forwardPorts": [9300],
        "portsAttributes": {
            "9300": {
                "label": "My App",
                "protocol": "https",
                "onAutoForward": "openPreview"
            }
        },
        "#,
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-show", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    let stdout = pod_command(&repo, &daemon)
        .args(["ports", "fwd-show"])
        .success()
        .expect("rumpel ports failed");

    let output = String::from_utf8_lossy(&stdout);
    assert!(output.contains("9300"), "Should show container port 9300");
    assert!(output.contains("My App"), "Should show label 'My App'");

    write_devcontainer_with_ports(
        &repo,
        r#"
        "forwardPorts": [9300],
        "portsAttributes": {
            "9300": {
                "label": "Changed App",
                "protocol": "http",
                "onAutoForward": "silent"
            }
        },
        "#,
    );

    let client = DaemonClient::new_unix(&daemon.socket_path);
    let ports = client
        .list_ports(
            PodName::new("fwd-show").expect("valid test pod name"),
            repo.path().to_path_buf(),
        )
        .expect("list forwarded port metadata");
    assert_eq!(ports.len(), 1, "expected one forwarded port");
    assert_eq!(ports[0].label, "My App");
    assert_eq!(
        ports[0].protocol,
        Some(PortProtocol::Https),
        "host edits changed the stored pod's port protocol"
    );
    assert_eq!(
        ports[0].on_auto_forward,
        Some(OnAutoForward::OpenPreview),
        "host edits changed the stored pod's auto-forward action"
    );
}

#[test]
fn ports_command_empty_when_no_forwards() {
    let repo = TestRepo::new();

    // No forwardPorts at all
    write_devcontainer_with_ports(&repo, "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-empty-cfg", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    let stdout = pod_command(&repo, &daemon)
        .args(["ports", "fwd-empty-cfg"])
        .success()
        .expect("rumpel ports failed");

    let output = String::from_utf8_lossy(&stdout);
    // Should show the header but no port rows
    assert!(
        !output.contains("9"),
        "Should not show any ports when none are forwarded"
    );
}

#[test]
fn other_ports_attributes_label() {
    let repo = TestRepo::new();

    // Port 9350 is in forwardPorts but NOT in portsAttributes,
    // so it should pick up the label from otherPortsAttributes.
    write_devcontainer_with_ports(
        &repo,
        r#"
        "forwardPorts": [9350],
        "portsAttributes": {
            "9999": { "label": "Explicit" }
        },
        "otherPortsAttributes": { "label": "Fallback" },
        "#,
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-other-attr", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    let stdout = pod_command(&repo, &daemon)
        .args(["ports", "fwd-other-attr"])
        .success()
        .expect("rumpel ports failed");

    let output = String::from_utf8_lossy(&stdout);
    assert!(
        output.contains("9350"),
        "Should show container port 9350, got: {}",
        output
    );
    assert!(
        output.contains("Fallback"),
        "Port not in portsAttributes should get otherPortsAttributes label, got: {}",
        output
    );
}

#[test]
fn multi_pod_port_remapping() {
    let repo = TestRepo::new();

    write_devcontainer_with_ports(&repo, r#""forwardPorts": [9400],"#);
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // First pod gets port 9400
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-first", "--", "true"])
        .success()
        .expect("rumpel enter failed for first pod");

    // Second pod also wants port 9400 but should get remapped
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-second", "--", "true"])
        .success()
        .expect("rumpel enter failed for second pod");

    // Check the ports for both pods
    let stdout1 = pod_command(&repo, &daemon)
        .args(["ports", "fwd-first"])
        .success()
        .expect("rumpel ports failed for first");
    let output1 = String::from_utf8_lossy(&stdout1);

    let stdout2 = pod_command(&repo, &daemon)
        .args(["ports", "fwd-second"])
        .success()
        .expect("rumpel ports failed for second");
    let output2 = String::from_utf8_lossy(&stdout2);

    // Both should show container port 9400
    assert!(output1.contains("9400"), "First pod should show port 9400");
    assert!(
        output2.contains("9400"),
        "Second pod should show container port 9400"
    );

    // Extract local ports from each - they must differ
    let local_port_1 = extract_local_port(&output1, 9400);
    let local_port_2 = extract_local_port(&output2, 9400);

    assert_ne!(
        local_port_1, local_port_2,
        "Two pods should not share the same local port for 9400"
    );
}

/// Extract the local port for a given container port from `rumpel ports` output.
/// The output format is: CONTAINER  LOCAL  LABEL
fn extract_local_port(output: &str, container_port: u16) -> u16 {
    let port_str = container_port.to_string();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 && fields[0] == port_str {
            return fields[1].parse().expect("local port should be a number");
        }
    }
    panic!(
        "Could not find container port {} in output:\n{}",
        container_port, output
    );
}

#[test]
fn forward_port_subcommand_adds_runtime_forward() {
    let repo = TestRepo::new();

    // No forwardPorts in devcontainer.json -- the forward must come
    // from `rumpel forward-port` exclusively.
    write_devcontainer_with_ports(&repo, "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fp-add", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    // Auto-pick local port near the container port.
    let stdout = pod_command(&repo, &daemon)
        .args(["forward-port", "fp-add:9600"])
        .success()
        .expect("rumpel forward-port failed");
    let output = String::from_utf8_lossy(&stdout);
    assert!(
        output.contains("forwarded container port 9600"),
        "unexpected forward-port output: {output}"
    );

    start_echo_server_in_container(&repo, &daemon, "fp-add", 9600);

    let response = try_echo(9600, "manual forward");
    assert_eq!(
        response.as_deref(),
        Some("manual forward"),
        "Expected echo response through manual forward port 9600"
    );

    // Duplicate add must fail with a friendly message and not crash.
    let dup = pod_command(&repo, &daemon)
        .args(["forward-port", "fp-add:9600"])
        .output()
        .expect("running rumpel forward-port");
    assert!(
        !dup.status.success(),
        "duplicate forward-port should fail, got status {:?}",
        dup.status
    );
    let stderr = String::from_utf8_lossy(&dup.stderr).into_owned();
    assert!(
        stderr.contains("already forwarded"),
        "expected duplicate-add error, got: {stderr}"
    );
}

#[test]
fn forward_port_persists_across_pod_restart() {
    let repo = TestRepo::new();

    // Devcontainer declares no forwardPorts; the only forward comes
    // from `rumpel forward-port`, which must survive a stop/restart.
    write_devcontainer_with_ports(&repo, "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fp-persist", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    pod_command(&repo, &daemon)
        .args(["forward-port", "fp-persist:9800"])
        .success()
        .expect("rumpel forward-port failed");

    pod_command(&repo, &daemon)
        .args(["stop", "--wait", "fp-persist"])
        .success()
        .expect("rumpel stop failed");

    pod_command(&repo, &daemon)
        .args(["enter", "fp-persist", "--", "true"])
        .success()
        .expect("rumpel enter (restart) failed");

    start_echo_server_in_container(&repo, &daemon, "fp-persist", 9800);

    // The forward must be live again on the same local port (port 9800
    // since that is how `bind_near` originally chose it for an empty
    // allocation set).
    let stdout = pod_command(&repo, &daemon)
        .args(["ports", "fp-persist"])
        .success()
        .expect("rumpel ports failed");
    let listing = String::from_utf8_lossy(&stdout);
    let local_port = extract_local_port(&listing, 9800);

    let response = try_echo(local_port, "still here");
    assert_eq!(
        response.as_deref(),
        Some("still here"),
        "expected manual forward to survive pod restart, got {response:?}"
    );
}

#[test]
fn forward_port_is_restored_by_connect_and_survives_pod_repair() {
    if !matches!(executor::executor_mode(), executor::ExecutorMode::Docker) {
        executor::skip_test();
        return;
    }
    let repo = TestRepo::new();

    write_devcontainer_with_ports(&repo, "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let mut daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fp-connect", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    pod_command(&repo, &daemon)
        .args(["forward-port", "fp-connect:9850"])
        .success()
        .expect("rumpel forward-port failed");

    start_echo_server_in_container(&repo, &daemon, "fp-connect", 9850);

    let stdout = pod_command(&repo, &daemon)
        .args(["ports", "fp-connect"])
        .success()
        .expect("rumpel ports failed before connect");
    let listing = String::from_utf8_lossy(&stdout);
    let local_port = extract_local_port(&listing, 9850);

    let response = try_echo(local_port, "before connect");
    assert_eq!(
        response.as_deref(),
        Some("before connect"),
        "expected forward to work before connect, got {response:?}"
    );

    daemon.kill();
    let daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["connect", "fp-connect"])
        .success()
        .expect("rumpel connect after daemon restart failed");

    let response = try_echo(local_port, "after daemon restart");
    assert_eq!(
        response.as_deref(),
        Some("after daemon restart"),
        "expected connect to restore the forward, got {response:?}"
    );

    stop_container_server("fp-connect");
    pod_command(&repo, &daemon)
        .args(["connect", "fp-connect"])
        .success()
        .expect("rumpel connect failed");

    let stdout = pod_command(&repo, &daemon)
        .args(["ports", "fp-connect"])
        .success()
        .expect("rumpel ports failed after connect");
    let listing = String::from_utf8_lossy(&stdout);
    let connected_local_port = extract_local_port(&listing, 9850);
    assert_eq!(
        connected_local_port, local_port,
        "connect changed the persisted local port"
    );

    let response = try_echo(connected_local_port, "after connect");
    assert_eq!(
        response.as_deref(),
        Some("after connect"),
        "expected forward to work after connect, got {response:?}"
    );
}

#[test]
fn forward_port_subcommand_explicit_local_port() {
    let repo = TestRepo::new();

    write_devcontainer_with_ports(&repo, "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fp-explicit", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    let stdout = pod_command(&repo, &daemon)
        .args(["forward-port", "fp-explicit:9700", "9701"])
        .success()
        .expect("rumpel forward-port failed");
    let output = String::from_utf8_lossy(&stdout);
    assert!(
        output.contains("local port 9701"),
        "expected explicit local port in output: {output}"
    );

    start_echo_server_in_container(&repo, &daemon, "fp-explicit", 9700);

    let response = try_echo(9701, "explicit local");
    assert_eq!(
        response.as_deref(),
        Some("explicit local"),
        "Expected echo response through explicit local port 9701"
    );
}

#[test]
fn forward_port_remote_ssh() {
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker
    ) {
        crate::executor::skip_test();
        return;
    }
    let repo = TestRepo::new();

    // Install socat before switching to testuser so the install runs as root.
    write_test_devcontainer(
        &repo,
        "USER root\nRUN apk add --no-cache socat\nUSER testuser\n",
        r#","forwardPorts": [9500]"#,
    );

    let home = TestHome::new();
    let executor = ExecutorResources::ssh(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fwd-remote", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    // Start an echo server on port 9500 inside the remote container
    start_echo_server_in_container(&repo, &daemon, "fwd-remote", 9500);

    // The port should be forwarded from localhost to the container's
    // port 9500 on the remote host's Docker network.
    let stdout = pod_command(&repo, &daemon)
        .args(["ports", "fwd-remote"])
        .success()
        .expect("rumpel ports failed");
    let output = String::from_utf8_lossy(&stdout);
    assert!(
        output.contains("9500"),
        "Should show forwarded port 9500, got: {}",
        output
    );

    // Verify data flows through the SSH-tunnelled forward
    let response = try_echo(9500, "hello remote");
    assert_eq!(
        response.as_deref(),
        Some("hello remote"),
        "Expected echo response through SSH-forwarded port 9500"
    );
}
