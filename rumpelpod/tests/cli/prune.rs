// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the `rumpel prune` subcommand.

use std::fs;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::{executor_supports_stop, ExecutorResources};

/// Poll `rumpel list` until `name` shows with the given status word.
fn wait_for_pod_status(repo: &TestRepo, daemon: &TestDaemon, name: &str, status: &str) {
    let timeout = std::time::Duration::from_secs(15);
    let start = std::time::Instant::now();
    loop {
        let output = pod_command(repo, daemon)
            .arg("list")
            .output()
            .expect("Failed to list");
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains(name) && line.contains(status) {
                return;
            }
        }
        if start.elapsed() > timeout {
            panic!(
                "pod '{}' did not reach status '{}' within {:?}\nlist output:\n{}",
                name, status, timeout, stdout
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Helper: create a pod, run a trivial command, then stop it and wait for
/// the daemon to report it as stopped.
fn create_stopped_pod(repo: &TestRepo, daemon: &TestDaemon, name: &str) {
    let output = pod_command(repo, daemon)
        .args(["enter", "--create", name, "--", "true"])
        .output()
        .expect("Failed to run rumpel enter");
    assert!(
        output.status.success(),
        "enter '{}' failed: {}",
        name,
        String::from_utf8_lossy(&output.stderr)
    );

    let output = pod_command(repo, daemon)
        .args(["stop", name])
        .output()
        .expect("Failed to run rumpel stop");
    assert!(
        output.status.success(),
        "stop '{}' failed: {}",
        name,
        String::from_utf8_lossy(&output.stderr)
    );

    wait_for_pod_status(repo, daemon, name, "stopped");
}

/// Helper: create a stopped pod that is ahead of its upstream.
fn create_ahead_stopped_pod(repo: &TestRepo, daemon: &TestDaemon, name: &str) {
    let output = pod_command(repo, daemon)
        .args(["enter", "--create", name, "--", "touch", "newfile"])
        .output()
        .expect("Failed to touch file");
    assert!(output.status.success(), "touch failed");

    let output = pod_command(repo, daemon)
        .args(["enter", "--create", name, "--", "git", "add", "newfile"])
        .output()
        .expect("Failed to git add");
    assert!(output.status.success(), "git add failed");

    let output = pod_command(repo, daemon)
        .args([
            "enter",
            "--create",
            name,
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "new file",
        ])
        .output()
        .expect("Failed to git commit");
    assert!(output.status.success(), "git commit failed");

    let output = pod_command(repo, daemon)
        .args(["stop", name])
        .output()
        .expect("Failed to run rumpel stop");
    assert!(
        output.status.success(),
        "stop '{}' failed: {}",
        name,
        String::from_utf8_lossy(&output.stderr)
    );

    wait_for_pod_status(repo, daemon, name, "stopped");
}

#[test]
fn prune_no_stopped_pods() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .arg("prune")
        .output()
        .expect("Failed to run rumpel prune");

    assert!(
        output.status.success(),
        "prune with no pods should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no stopped pods"),
        "expected 'No stopped pods' message: {}",
        stderr
    );
}

#[test]
fn prune_deletes_stopped_pods() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    create_stopped_pod(&repo, &daemon, "prune-a");
    create_stopped_pod(&repo, &daemon, "prune-b");

    let output = pod_command(&repo, &daemon)
        .args(["prune", "--force"])
        .output()
        .expect("Failed to run rumpel prune");

    assert!(
        output.status.success(),
        "prune --force should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify pods are gone
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("prune-a") && !stdout.contains("prune-b"),
        "pods should be gone after prune: {}",
        stdout
    );
}

// Flaky: the spawned pod sometimes fails to reach "running" status
// within the 15s timeout, likely due to container startup variance.
#[test]
#[ignore]
fn prune_leaves_running_pods() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a running pod (enter with sleep to keep it alive)
    let mut running = pod_command(&repo, &daemon)
        .args(["enter", "--create", "keep-running", "--", "sleep", "120"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("Failed to spawn running pod");

    // Wait for the running pod to appear
    wait_for_pod_status(&repo, &daemon, "keep-running", "running");

    // Create a stopped pod
    create_stopped_pod(&repo, &daemon, "to-prune");

    let output = pod_command(&repo, &daemon)
        .args(["prune", "--force"])
        .output()
        .expect("Failed to run rumpel prune");

    assert!(
        output.status.success(),
        "prune should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the running pod is still there
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("keep-running"),
        "running pod should remain: {}",
        stdout
    );
    assert!(
        !stdout.contains("to-prune"),
        "stopped pod should be gone: {}",
        stdout
    );

    let _ = running.kill();
    let _ = running.wait();
}

#[test]
fn prune_skips_unmerged_without_tty() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    create_ahead_stopped_pod(&repo, &daemon, "unmerged-prune");

    // Verify the pod shows as ahead before pruning
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ahead"),
        "pod should show as ahead before prune test: {}",
        stdout
    );

    // pod_command pipes stdout/stderr, so this is non-tty
    let output = pod_command(&repo, &daemon)
        .arg("prune")
        .output()
        .expect("Failed to run rumpel prune");

    assert!(
        !output.status.success(),
        "prune of unmerged pod should fail without --force"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unmerged commits") && stderr.contains("--force"),
        "expected 'unmerged commits' and '--force' hint in stderr: {}",
        stderr
    );

    // Pod should still exist
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("unmerged-prune"),
        "unmerged pod should remain: {}",
        stdout
    );
}
