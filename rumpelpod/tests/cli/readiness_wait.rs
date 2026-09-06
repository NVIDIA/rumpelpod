// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A launch waiting for the container's SSE state greeting holds the pod's
//! lifecycle lock even after its CLI disconnects. These regressions require
//! subsequent delete and enter requests to recover without a daemon restart.
//! The startup hook supplies a reproducible stall; the original incident's
//! initial cause was unknown.

use std::fs;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use indoc::indoc;
use tempfile::NamedTempFile;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::{executor_mode, skip_test, ExecutorMode, ExecutorResources};

const RECOVERY_TIMEOUT: Duration = Duration::from_secs(15);

// A failed assertion must reap the CLI before TestDaemon removes the pods.
// Files keep diagnostics available without a pipe reader that could itself
// hang when an abandoned process retains stdout or stderr.
struct TestProcess {
    child: Child,
    log: NamedTempFile,
}

impl TestProcess {
    fn spawn(command: &mut Command) -> Self {
        let log = NamedTempFile::new().expect("create command log");
        let child = command
            .stdin(Stdio::null())
            .stdout(log.as_file().try_clone().expect("open stdout log"))
            .stderr(log.as_file().try_clone().expect("open stderr log"))
            .spawn()
            .expect("spawn test command");
        Self { child, log }
    }

    fn output(&self) -> String {
        fs::read_to_string(self.log.path()).expect("read command log")
    }

    fn wait_for_output(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let output = self.output();
            if output.contains(marker) {
                return;
            }
            assert!(
                self.child.try_wait().expect("poll command").is_none(),
                "command exited before {marker:?}:\n{output}"
            );
            assert!(
                Instant::now() < deadline,
                "command never reached {marker:?}:\n{output}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn success(&mut self, timeout: Duration, reason: &str) -> String {
        let status = self.wait(timeout, reason);
        let output = self.output();
        assert!(status.success(), "{reason}: {status}\n{output}");
        output
    }

    fn failure(&mut self, timeout: Duration, reason: &str) -> String {
        let status = self.wait(timeout, reason);
        let output = self.output();
        assert!(!status.success(), "{reason}: command succeeded\n{output}");
        output
    }

    fn wait(&mut self, timeout: Duration, reason: &str) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll command") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{reason}: command did not finish within {timeout:?}\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn abandon(&mut self) {
        assert!(
            self.child.try_wait().expect("poll launch").is_none(),
            "launch finished before its CLI could be abandoned:\n{}",
            self.output()
        );
        self.child.kill().expect("kill launching CLI");
        self.child.wait().expect("reap launching CLI");
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(e) => eprintln!("failed to poll test command during cleanup: {e}"),
        }
        if let Err(e) = self.child.kill() {
            eprintln!("failed to kill test command during cleanup: {e}");
        }
        if let Err(e) = self.child.wait() {
            eprintln!("failed to reap test command during cleanup: {e}");
        }
    }
}

fn requires_local_docker() -> bool {
    // These tests stop containers behind the daemon's back to model the
    // stopped containers left by the reboot in the reported incident.
    match executor_mode() {
        ExecutorMode::Docker => true,
        ExecutorMode::Podman | ExecutorMode::Ssh | ExecutorMode::K8s => {
            skip_test();
            false
        }
    }
}

fn blocked_launch(repo: &TestRepo, daemon: &TestDaemon, name: &str) -> (TestProcess, String) {
    // The persistent marker lets a restarted container finish setup, so an
    // enter timeout after stopping it cannot be blamed on the hook rerunning.
    write_test_devcontainer(
        repo,
        "",
        indoc! {r#",
            "onCreateCommand": "if [ ! -e /tmp/readiness-started ]; then touch /tmp/readiness-started; while [ ! -e /tmp/release-readiness ]; do sleep 0.1; done; fi",
            "waitFor": "onCreateCommand"
        "#},
    );
    let mut launch = TestProcess::spawn(pod_command(repo, daemon).args([
        "enter",
        "--create",
        name,
        "--",
        "echo",
        "readiness-enter-ok",
    ]));

    // This message reaches the CLI through wait_ready_impl's /events reader,
    // proving the launch has reached readiness polling with its lock held.
    launch.wait_for_output("running lifecycle commands...");
    let repo_path = repo.path().display();
    let output = TestProcess::spawn(Command::new("docker").args([
        "ps",
        "-q",
        "--filter",
        &format!("label=dev.rumpelpod.repo_path={repo_path}"),
        "--filter",
        &format!("label=dev.rumpelpod.name={name}"),
    ]))
    .success(RECOVERY_TIMEOUT, "find the test container");
    let ids: Vec<_> = output.lines().collect();
    assert_eq!(
        ids.len(),
        1,
        "expected exactly one test container: {output}"
    );
    let container_id = ids[0].to_string();
    TestProcess::spawn(Command::new("docker").args([
        "exec",
        &container_id,
        "sh",
        "-c",
        "while [ ! -e /tmp/readiness-started ]; do sleep 0.1; done",
    ]))
    .success(RECOVERY_TIMEOUT, "startup hook did not reach its barrier");
    (launch, container_id)
}

fn stop_container(container_id: &str) {
    // rumpel stop needs the same lock as launch and would hide the readiness
    // retry bug behind another lock waiter. Kill only this test's container.
    TestProcess::spawn(Command::new("docker").args(["kill", container_id]))
        .success(RECOVERY_TIMEOUT, "stop container outside the daemon");
    let output = TestProcess::spawn(Command::new("docker").args([
        "inspect",
        "--format",
        "{{.State.Running}}",
        container_id,
    ]))
    .success(RECOVERY_TIMEOUT, "inspect stopped container");
    assert_eq!(output.trim(), "false");
}

#[test]
fn readiness_wait_completes_when_lifecycle_is_released() {
    if !requires_local_docker() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let (mut launch, container_id) = blocked_launch(&repo, &daemon, "release-readiness");

    // This exceeds the readiness read deadline and the lifecycle guard's
    // first progress message. Only startup heartbeats keep the stream live
    // until the next progress message at 70 seconds.
    std::thread::sleep(Duration::from_secs(45));
    assert!(
        launch
            .child
            .try_wait()
            .expect("poll live startup")
            .is_none(),
        "healthy setup timed out before the hook was released:\n{}",
        launch.output()
    );
    TestProcess::spawn(Command::new("docker").args([
        "exec",
        &container_id,
        "touch",
        "/tmp/release-readiness",
    ]))
    .success(RECOVERY_TIMEOUT, "release the startup hook");
    let output = launch.success(RECOVERY_TIMEOUT, "readiness should complete after release");
    assert!(output.contains("readiness-enter-ok"), "{output}");
    TestProcess::spawn(pod_command(&repo, &daemon).args([
        "delete",
        "--force",
        "--wait",
        "release-readiness",
    ]))
    .success(RECOVERY_TIMEOUT, "completed launch should release the lock");
}

#[test]
fn readiness_wait_delete_cancels_abandoned_launch() {
    if !requires_local_docker() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let (mut launch, _container_id) = blocked_launch(&repo, &daemon, "delete-readiness");
    launch.abandon();

    // /events remains open without a state greeting. Deletion must be able
    // to interrupt that read rather than wait for setup to finish.
    TestProcess::spawn(pod_command(&repo, &daemon).args([
        "delete",
        "--force",
        "--wait",
        "delete-readiness",
    ]))
    .success(
        RECOVERY_TIMEOUT,
        "delete blocked behind the abandoned launch's readiness wait",
    );
    let output = TestProcess::spawn(pod_command(&repo, &daemon).arg("list"))
        .success(RECOVERY_TIMEOUT, "list after deletion");
    assert!(!output.contains("delete-readiness"), "{output}");
}

#[test]
fn readiness_wait_delete_cancels_abandoned_launch_in_stopped_container() {
    if !requires_local_docker() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let (mut launch, container_id) = blocked_launch(&repo, &daemon, "delete-stopped-readiness");
    launch.abandon();
    stop_container(&container_id);

    // The stopped server can never supply a greeting. Retrying its endpoint
    // indefinitely must not prevent removal of the abandoned pod.
    TestProcess::spawn(pod_command(&repo, &daemon).args([
        "delete",
        "--force",
        "--wait",
        "delete-stopped-readiness",
    ]))
    .success(
        RECOVERY_TIMEOUT,
        "delete blocked behind readiness retries for a stopped container",
    );
    let output = TestProcess::spawn(pod_command(&repo, &daemon).arg("list"))
        .success(RECOVERY_TIMEOUT, "list after deletion");
    assert!(!output.contains("delete-stopped-readiness"), "{output}");
}

#[test]
fn readiness_wait_enter_recovers_abandoned_launch_in_stopped_container() {
    if !requires_local_docker() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let (mut launch, container_id) = blocked_launch(&repo, &daemon, "enter-stopped-readiness");
    launch.abandon();
    stop_container(&container_id);

    let output = TestProcess::spawn(pod_command(&repo, &daemon).args([
        "enter",
        "enter-stopped-readiness",
        "--",
        "echo",
        "readiness-recovered",
    ]))
    .success(
        RECOVERY_TIMEOUT,
        "enter blocked behind readiness retries instead of restarting the stopped container",
    );
    assert!(output.contains("readiness-recovered"), "{output}");
}

#[test]
fn readiness_wait_reports_stopped_container_to_launching_cli() {
    if !requires_local_docker() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let (mut launch, container_id) = blocked_launch(&repo, &daemon, "failed-readiness");

    stop_container(&container_id);
    let output = launch.failure(
        RECOVERY_TIMEOUT,
        "a stopped container should fail its launch without another command cancelling it",
    );
    assert!(output.contains("event stream"), "{output}");
}

#[test]
fn readiness_wait_delete_cancels_attached_launch_with_unresponsive_server() {
    if !requires_local_docker() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let (mut launch, container_id) = blocked_launch(&repo, &daemon, "paused-readiness");

    // A paused server cannot send progress or close its stream. Delete must
    // interrupt the pending read even while the original CLI is still alive.
    TestProcess::spawn(Command::new("docker").args(["pause", &container_id]))
        .success(RECOVERY_TIMEOUT, "pause the test container");
    TestProcess::spawn(pod_command(&repo, &daemon).args([
        "delete",
        "--force",
        "--wait",
        "paused-readiness",
    ]))
    .success(
        RECOVERY_TIMEOUT,
        "delete must interrupt an unresponsive readiness stream",
    );
    let output = launch.failure(RECOVERY_TIMEOUT, "delete must cancel the original launch");
    assert!(
        output.contains("readiness wait cancelled by delete"),
        "{output}"
    );
}

#[test]
fn readiness_wait_reports_unresponsive_server_to_launching_cli() {
    if !requires_local_docker() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let (mut launch, container_id) = blocked_launch(&repo, &daemon, "silent-readiness");

    TestProcess::spawn(Command::new("docker").args(["pause", &container_id]))
        .success(RECOVERY_TIMEOUT, "pause the test container");
    let output = launch.failure(
        Duration::from_secs(45),
        "a server that stops sending heartbeats should fail its launch",
    );
    assert!(output.contains("event stream"), "{output}");
    TestProcess::spawn(pod_command(&repo, &daemon).args([
        "delete",
        "--force",
        "--wait",
        "silent-readiness",
    ]))
    .success(
        RECOVERY_TIMEOUT,
        "readiness timeout should release the lock",
    );
}

#[test]
fn readiness_wait_delete_cancels_queued_enter_without_recreating_pod() {
    if !requires_local_docker() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let (mut launch, _container_id) = blocked_launch(&repo, &daemon, "queued-readiness");
    launch.abandon();

    let mut enter = TestProcess::spawn(pod_command(&repo, &daemon).args([
        "enter",
        "queued-readiness",
        "--",
        "true",
    ]));
    enter.wait_for_output("waiting for another operation on this pod");
    TestProcess::spawn(pod_command(&repo, &daemon).args([
        "delete",
        "--force",
        "--wait",
        "queued-readiness",
    ]))
    .success(
        RECOVERY_TIMEOUT,
        "delete must take priority over queued enter",
    );
    let output = enter.failure(
        RECOVERY_TIMEOUT,
        "queued enter must not recreate a deleted pod",
    );
    assert!(
        output.contains("pod operation cancelled by delete"),
        "{output}"
    );
    let output = TestProcess::spawn(pod_command(&repo, &daemon).arg("list"))
        .success(RECOVERY_TIMEOUT, "list after cancelling the queued enter");
    assert!(!output.contains("queued-readiness"), "{output}");
}
