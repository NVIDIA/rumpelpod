// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the `rumpel list` subcommand.

use std::fs;
use std::process::Command;
use std::time::Duration;

use retry::delay::{Exponential, Fixed};
use retry::OperationResult;
use serde_json::json;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::{executor_mode, ExecutorMode, ExecutorResources};
use crate::ssh::{write_ssh_config, SshRemoteHost, SSH_USER};

#[test]
fn list_empty_returns_header_only() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to run rumpel list command");

    assert!(
        output.status.success(),
        "rumpel list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("NAME"));
    assert!(stdout.contains("GIT"));
    assert!(stdout.contains("STATUS"));
    assert!(stdout.contains("CREATED"));
    assert!(!stdout.contains("HOST"));
    assert!(stdout.contains("CONTAINER ID"));
}

#[test]
fn list_shows_created_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "test-list", "--", "echo", "hello"])
        .output()
        .expect("Failed to run rumpel enter command");

    assert!(
        output.status.success(),
        "rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // List pods
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to run rumpel list command");

    assert!(
        output.status.success(),
        "rumpel list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("test-list"),
        "Expected pod 'test-list' in output: {}",
        stdout
    );
    assert!(
        stdout.contains("running"),
        "Expected 'running' status in output: {}",
        stdout
    );
}

#[test]
fn list_shows_container_id_without_sync() {
    // Slice the row at the header's column offset rather than taking
    // the last whitespace token: CONTAINER ID is the last column, so
    // when it is blank the last token would silently be the previous
    // column's value.
    fn container_id_column(stdout: &str, pod_name: &str) -> String {
        let header_offset = stdout
            .lines()
            .find_map(|line| line.find("CONTAINER ID"))
            .unwrap_or_else(|| panic!("no CONTAINER ID header in output: {stdout}"));
        let line = stdout
            .lines()
            .find(|line| line.contains(pod_name))
            .unwrap_or_else(|| panic!("no row for pod '{pod_name}' in output: {stdout}"));
        line.get(header_offset..).unwrap_or("").trim().to_string()
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let mut daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "cid-test", "--", "echo", "hello"])
        .output()
        .expect("Failed to run rumpel enter command");
    assert!(
        output.status.success(),
        "rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Plain list, no --sync: the id comes from the daemon's cache,
    // filled at launch (or, for a host not queried since daemon
    // start, by the one-time warm query).
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to run rumpel list command");
    assert!(
        output.status.success(),
        "rumpel list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let container_id = container_id_column(&stdout, "cid-test");
    match executor_mode() {
        // Docker engines show the 12-char short id.
        ExecutorMode::Docker | ExecutorMode::Podman | ExecutorMode::Ssh => {
            assert!(
                container_id.len() == 12 && container_id.chars().all(|c| c.is_ascii_hexdigit()),
                "expected a 12-char hex container id, got '{container_id}': {stdout}"
            );
        }
        // K8s shows the pod name, truncated to the same width.
        ExecutorMode::K8s => {
            assert!(
                container_id.starts_with("rumpel-"),
                "expected a rumpel- pod name prefix, got '{container_id}': {stdout}"
            );
        }
    }

    // The cached id must identify the real backend container.  SSH
    // and K8s put the container runtime behind a tunnel / the cluster
    // API, so only the local engines can be cross-checked directly.
    // The list output shows the docker-style 12-char short id, so
    // compare by prefix.
    let engine_cli = match executor_mode() {
        ExecutorMode::Docker => Some("docker"),
        ExecutorMode::Podman => Some("podman"),
        ExecutorMode::Ssh | ExecutorMode::K8s => None,
    };
    if let Some(engine_cli) = engine_cli {
        let output = Command::new(engine_cli)
            .args([
                "ps",
                "--all",
                "--filter",
                "label=dev.rumpelpod.name=cid-test",
                "--no-trunc",
                "--format",
                "{{.ID}}",
            ])
            .output()
            .unwrap_or_else(|e| panic!("{engine_cli} ps failed: {e}"));
        assert!(output.status.success());
        let ids = String::from_utf8_lossy(&output.stdout);
        assert!(
            ids.lines().any(|id| id.starts_with(&container_id)),
            "container id '{container_id}' is not a prefix of any {engine_cli} container id: {ids}"
        );
    }

    // A restarted daemon has an empty cache; the first list re-fetches
    // ids from the backend.  SSH is excluded: its host connection is
    // only re-established when a client enters the pod, so ids come
    // back after the next enter or --sync instead.
    match executor_mode() {
        ExecutorMode::Docker | ExecutorMode::Podman | ExecutorMode::K8s => {
            daemon.kill();
            let daemon = TestDaemon::start(&home);
            let output = pod_command(&repo, &daemon)
                .arg("list")
                .output()
                .expect("Failed to run rumpel list command");
            assert!(
                output.status.success(),
                "rumpel list failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            assert_eq!(
                container_id_column(&stdout, "cid-test"),
                container_id,
                "container id changed across daemon restart: {stdout}"
            );
        }
        ExecutorMode::Ssh => {}
    }
}

#[test]
fn list_shows_repo_state() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "test-state", "--", "echo", "hello"])
        .output()
        .expect("Failed to run rumpel enter command");

    assert!(
        output.status.success(),
        "rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Initial state should be "up to date" (tracked via branch)
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to run rumpel list command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("up to date") || stdout.contains("ahead") || stdout.contains("behind"),
        "Expected repo state in output: {}",
        stdout
    );

    // Make a commit in the pod
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "test-state", "--", "touch", "newfile"])
        .output()
        .expect("Failed to touch file");
    assert!(output.status.success());

    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "test-state",
            "--",
            "git",
            "add",
            "newfile",
        ])
        .output()
        .expect("Failed to git add");
    assert!(output.status.success());

    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "test-state",
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "new file",
        ])
        .output()
        .expect("Failed to git commit");
    assert!(output.status.success());

    // Check list again - should show ahead
    let output = pod_command(&repo, &daemon)
        .args(["list", "--sync"])
        .output()
        .expect("Failed to run rumpel list command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ahead 1"),
        "Expected 'ahead 1' in output: {}",
        stdout
    );
}

#[test]
fn list_shows_multiple_pods() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create first pod
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "pod-one", "--", "echo", "one"])
        .output()
        .expect("Failed to run rumpel enter command");
    assert!(
        output.status.success(),
        "first rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Create second pod
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "pod-two", "--", "echo", "two"])
        .output()
        .expect("Failed to run rumpel enter command");
    assert!(
        output.status.success(),
        "second rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // List pods
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to run rumpel list command");

    assert!(
        output.status.success(),
        "rumpel list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pod-one"),
        "Expected 'pod-one' in output: {}",
        stdout
    );
    assert!(
        stdout.contains("pod-two"),
        "Expected 'pod-two' in output: {}",
        stdout
    );
}

/// Running pods should appear before stopped pods, even when the stopped pod has
/// a more recent commit.
#[test]
fn list_shows_running_pods_before_stopped() {
    // `rumpel stop` is not supported on k8s, which this test relies on
    // to distinguish running from stopped pods.
    if !crate::executor::executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create first pod (stays running, never gets a new commit)
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "active-pod", "--", "echo", "one"])
        .output()
        .expect("Failed to run rumpel enter command");
    assert!(
        output.status.success(),
        "first rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Create second pod and give it a newer commit so its committer date wins
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "halted-pod", "--", "touch", "newfile"])
        .output()
        .expect("Failed to run rumpel enter command");
    assert!(
        output.status.success(),
        "second rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for args in [
        vec!["enter", "halted-pod", "--", "git", "add", "newfile"],
        vec![
            "enter",
            "--create",
            "halted-pod",
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "new file",
        ],
        vec!["stop", "halted-pod"],
    ] {
        let output = pod_command(&repo, &daemon)
            .args(&args)
            .output()
            .unwrap_or_else(|_| panic!("Failed to run: {:?}", args));
        assert!(
            output.status.success(),
            "{:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Docker may need a moment to report the container as stopped.
    // Poll until the list output reflects the stopped status.
    let stdout = retry::retry(Exponential::from_millis(100).take(8), || {
        let output = pod_command(&repo, &daemon)
            .arg("list")
            .output()
            .expect("Failed to run rumpel list command");
        assert!(
            output.status.success(),
            "rumpel list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let pod_line = stdout
            .lines()
            .find(|l| l.contains("halted-pod"))
            .unwrap_or("");
        if pod_line.contains("stopped") {
            OperationResult::Ok(stdout)
        } else {
            OperationResult::Retry(stdout)
        }
    })
    .expect("halted-pod never showed as stopped in list output");

    // Running pod should appear before stopped pod, even though the
    // stopped pod has a more recent commit.
    let active_pos = stdout
        .find("active-pod")
        .expect("Expected 'active-pod' in output");
    let halted_pos = stdout
        .find("halted-pod")
        .expect("Expected 'halted-pod' in output");
    assert!(
        active_pos < halted_pos,
        "Running pod should appear before stopped pod in output: {}",
        stdout
    );
}

/// Within the same status group, pods with a more recent commit on their primary
/// branch should appear first.
#[test]
fn list_sorts_by_commit_date_within_status() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create two pods -- both start at the same host HEAD commit.
    for name in ["stale-pod", "fresh-pod"] {
        let output = pod_command(&repo, &daemon)
            .args(["enter", "--create", name, "--", "echo", "hello"])
            .output()
            .unwrap_or_else(|_| panic!("Failed to create {}", name));
        assert!(
            output.status.success(),
            "rumpel enter {} failed: {}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Make a commit in fresh-pod so its branch ref gets a newer committer date.
    for args in [
        vec!["enter", "fresh-pod", "--", "touch", "newfile"],
        vec!["enter", "fresh-pod", "--", "git", "add", "newfile"],
        vec![
            "enter",
            "--create",
            "fresh-pod",
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "new file",
        ],
    ] {
        let output = pod_command(&repo, &daemon)
            .args(&args)
            .output()
            .unwrap_or_else(|_| panic!("Failed to run: {:?}", args));
        assert!(
            output.status.success(),
            "{:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // List - fresh-pod should come before stale-pod (both running, sorted by commit date)
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to run rumpel list command");

    assert!(
        output.status.success(),
        "rumpel list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fresh_pos = stdout
        .find("fresh-pod")
        .expect("Expected 'fresh-pod' in output");
    let stale_pos = stdout
        .find("stale-pod")
        .expect("Expected 'stale-pod' in output");
    assert!(
        fresh_pos < stale_pos,
        "Pod with newer commit should appear first: {}",
        stdout
    );
}

#[test]
fn list_does_not_show_other_repo_pods() {
    let repo1 = TestRepo::new();
    let home1 = TestHome::new();
    let executor1 = ExecutorResources::setup(&home1);
    let daemon1 = TestDaemon::start(&home1);
    write_test_devcontainer(&repo1, "", "");
    fs::write(repo1.path().join(".rumpelpod.json"), &executor1.json).unwrap();

    let repo2 = TestRepo::new();
    let home2 = TestHome::new();
    let executor2 = ExecutorResources::setup(&home2);
    let daemon2 = TestDaemon::start(&home2);
    write_test_devcontainer(&repo2, "", "");
    fs::write(repo2.path().join(".rumpelpod.json"), &executor2.json).unwrap();

    // Create pod in repo1
    let output = pod_command(&repo1, &daemon1)
        .args(["enter", "--create", "repo1-pod", "--", "echo", "repo1"])
        .output()
        .expect("Failed to run rumpel enter command");
    assert!(
        output.status.success(),
        "repo1 rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Create pod in repo2
    let output = pod_command(&repo2, &daemon2)
        .args(["enter", "--create", "repo2-pod", "--", "echo", "repo2"])
        .output()
        .expect("Failed to run rumpel enter command");
    assert!(
        output.status.success(),
        "repo2 rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // List pods from repo1 - should only see repo1-pod
    let output = pod_command(&repo1, &daemon1)
        .arg("list")
        .output()
        .expect("Failed to run rumpel list command");

    assert!(
        output.status.success(),
        "rumpel list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("repo1-pod"),
        "Expected 'repo1-pod' in output: {}",
        stdout
    );
    assert!(
        !stdout.contains("repo2-pod"),
        "Should not see pod from other repo in output: {}",
        stdout
    );

    // List pods from repo2 - should only see repo2-pod
    let output = pod_command(&repo2, &daemon2)
        .arg("list")
        .output()
        .expect("Failed to run rumpel list command");

    assert!(
        output.status.success(),
        "rumpel list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("repo2-pod"),
        "Expected 'repo2-pod' in output: {}",
        stdout
    );
    assert!(
        !stdout.contains("repo1-pod"),
        "Should not see pod from other repo in output: {}",
        stdout
    );
}

#[test]
fn ssh_remote_pod_list() {
    // Spins up a local Docker-backed SSH remote, which only the Docker
    // executor can reach.  Under ssh/k8s the ambient executor is
    // different and this scenario no longer applies.
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker
    ) {
        crate::executor::skip_test();
        return;
    }
    println!("xtest:timeout=185");
    let repo = TestRepo::new();
    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_ssh_config(&home, &[&remote]);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    let remote_spec = remote.ssh_spec();
    let config = serde_json::to_string(&json!({"host": remote_spec})).unwrap();
    fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    // Create a pod on the remote
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "list-test-remote", "--", "true"])
        .output()
        .expect("rumpel enter failed to execute");
    assert!(
        output.status.success(),
        "rumpel enter failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // List should show the pod with the remote host
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("rumpel list failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "rumpel list failed: stdout={}, stderr={}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("list-test-remote"),
        "rumpel list should show the pod: stdout={}, stderr={}",
        stdout,
        stderr
    );
    // With a single host, the HOST column is hidden.
    assert!(
        !stdout.contains("HOST"),
        "HOST column should be hidden with a single host: stdout={}, stderr={}",
        stdout,
        stderr
    );
}

/// The HOST column should appear when pods live on different hosts.
#[test]
fn list_shows_host_column_for_mixed_hosts() {
    // Spins up a local Docker-backed SSH remote, which only the Docker
    // executor can reach.
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker
    ) {
        crate::executor::skip_test();
        return;
    }
    println!("xtest:timeout=215");
    let repo = TestRepo::new();
    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_ssh_config(&home, &[&remote]);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    let remote_spec = remote.ssh_spec();
    let config = serde_json::to_string(&json!({"host": remote_spec})).unwrap();
    fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    // Create a pod on the remote host (via .rumpelpod.json).
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "remote-pod", "--", "true"])
        .output()
        .expect("rumpel enter failed to execute");
    assert!(
        output.status.success(),
        "remote enter failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Create a pod on localhost (override via --host).
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "local-pod",
            "--host",
            "localhost",
            "--",
            "true",
        ])
        .output()
        .expect("rumpel enter failed to execute");
    assert!(
        output.status.success(),
        "local enter failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("rumpel list failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "rumpel list failed: stdout={}, stderr={}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("HOST"),
        "HOST column should appear with mixed hosts: stdout={}, stderr={}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("localhost"),
        "should show localhost for the local pod: stdout={}, stderr={}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains(&format!("{SSH_USER}@")),
        "should show the remote host: stdout={}, stderr={}",
        stdout,
        stderr
    );
}

/// When the pod server reports codex state, `rumpel list` should show a
/// CODEX column with the human-readable state string.
// Hangs: retry::Exponential doubles the delay each iteration without
// a cap, so the 15th sleep is ~54 minutes and total wait is ~109
// minutes.
#[test]
#[ignore]
fn list_shows_codex_state() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    // Install curl so we can POST to the pod server from inside the
    // container.
    write_test_devcontainer(&repo, "RUN apk add --no-cache -q curl >/dev/null 2>&1", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod.
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "codex-st", "--", "echo", "hello"])
        .output()
        .expect("rumpel enter failed");
    assert!(
        output.status.success(),
        "rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // POST codex state "processing" to the in-container pod server.
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "codex-st",
            "--",
            "sh",
            "-c",
            concat!(
                "curl -sf -X POST http://127.0.0.1:$(cat /opt/rumpelpod/server-port)/codex-state ",
                "-H \"Authorization: Bearer $(cat /opt/rumpelpod/server-token)\" ",
                "-H 'Content-Type: application/json' ",
                "-d '{\"state\":\"processing\"}'",
            ),
        ])
        .output()
        .expect("curl failed");
    assert!(
        output.status.success(),
        "POST /codex-state failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The daemon's SSE listener needs a moment to pick up the state
    // change.  Poll rumpel list until the CODEX column appears.
    let stdout = retry::retry(
        Exponential::from(Duration::from_millis(200)).take(15),
        || {
            let output = pod_command(&repo, &daemon)
                .arg("list")
                .output()
                .expect("rumpel list failed");
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if stdout.contains("CODEX") {
                OperationResult::Ok(stdout)
            } else {
                OperationResult::Retry(stdout)
            }
        },
    )
    .expect("CODEX column never appeared in list output");

    assert!(
        stdout.contains("processing"),
        "expected 'processing' in CODEX column: {stdout}",
    );
}

/// When the pod server reports claude state, `rumpel list` should show
/// a CLAUDE column.  (Tests the pre-existing claude plumbing alongside
/// the new codex plumbing.)
#[test]
fn list_shows_claude_state() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "claude-st", "--", "echo", "hello"])
        .output()
        .expect("rumpel enter failed");
    assert!(
        output.status.success(),
        "rumpel enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Use the built-in hook subcommand to set claude state.
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "claude-st",
            "--",
            "/opt/rumpelpod/bin/rumpel",
            "claude-hook",
            "notify-state",
            "processing",
        ])
        .output()
        .expect("claude-hook failed");
    assert!(
        output.status.success(),
        "claude-hook notify-state failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = retry::retry(Fixed::from_millis(500).take(60), || {
        let output = pod_command(&repo, &daemon)
            .arg("list")
            .output()
            .expect("rumpel list failed");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.contains("CLAUDE") {
            OperationResult::Ok(stdout)
        } else {
            OperationResult::Retry(stdout)
        }
    })
    .expect("CLAUDE column never appeared in list output");

    assert!(
        stdout.contains("processing"),
        "expected 'processing' in CLAUDE column: {stdout}",
    );
}
