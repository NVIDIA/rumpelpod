// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! SSH reconnect cases that are closer to a laptop losing WiFi than
//! a clean `docker kill`.
//!
//! These start a nested SSH Docker host, several pods, and then kill
//! the transport in different ways. Recovery is asserted by a commit
//! made through `docker exec` (not SSH) showing up in the host repo
//! without `rumpel connect`.
//!
//! The ControlMaster / ServerAlive settings follow a typical laptop
//! SSH config; sshd `MaxSessions` is raised in the fixture image
//! because one mux master carries every pod route.
//!
//! Each case waits for `ssh -O check` to report a live mux after setup
//! and a dead mux after the fault, so recovery cannot hide behind a
//! ControlMaster that never went away.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indoc::formatdoc;

use super::{write_ssh_config, SshRemoteHost};
use crate::common::{
    pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo, TEST_REPO_PATH,
};
use crate::executor;
use rumpelpod::CommandExt;

const FAST_PROBE_MS: &str = "3000";

fn skip_if_unsupported() -> bool {
    if cfg!(target_os = "macos") {
        executor::skip_test();
        return true;
    }
    if !matches!(
        executor::executor_mode(),
        executor::ExecutorMode::Docker | executor::ExecutorMode::Ssh
    ) {
        executor::skip_test();
        return true;
    }
    false
}

struct MuxHarness {
    home: TestHome,
    remote: SshRemoteHost,
    /// Held so the daemon process stays up for the whole test.
    #[allow(dead_code)]
    daemon: TestDaemon,
    repo: TestRepo,
}

fn write_mux_ssh_config(home: &TestHome, remote: &SshRemoteHost) {
    write_ssh_config(home, &[remote]);
    let ssh_dir = home.path().join(".ssh");
    let sockets = ssh_dir.join("sockets");
    std::fs::create_dir_all(&sockets).expect("creating ssh mux socket dir");
    let known_hosts = ssh_dir.join("known_hosts");
    let control_path = sockets.join("%r@%h-%p");
    let mut config = formatdoc! {r#"
        Host *
            UserKnownHostsFile {known_hosts}
            StrictHostKeyChecking accept-new
            BatchMode yes
            ConnectTimeout 5
            IdentityAgent none
            ControlMaster auto
            ControlPath {control_path}
            ControlPersist 10m
            ServerAliveInterval 1
            ServerAliveCountMax 2
            TCPKeepAlive yes

        Host {ssh_host}
            IdentityFile {key}
            IdentitiesOnly yes
    "#,
        known_hosts = known_hosts.display(),
        control_path = control_path.display(),
        ssh_host = remote.ssh_host(),
        key = remote.private_key_path().display(),
    };
    if let Some(port) = remote.published_port {
        config.push_str(&format!("    Port {port}\n"));
    }
    std::fs::write(ssh_dir.join("config"), config).expect("writing mux ssh config");
}

fn start_mux_harness(pod_names: &[&str]) -> MuxHarness {
    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_mux_ssh_config(&home, &remote);
    let daemon =
        TestDaemon::start_with_env(&home, &[("RUMPELPOD_HOST_PROBE_TIMEOUT_MS", FAST_PROBE_MS)]);

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let config = serde_json::to_string(&serde_json::json!({"host": remote.ssh_spec()})).unwrap();
    std::fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    for name in pod_names {
        pod_command(&repo, &daemon)
            .args(["enter", "--create", name, "--", "true"])
            .success()
            .unwrap_or_else(|e| panic!("creating pod {name} failed: {e:#}"));
    }

    let harness = MuxHarness {
        home,
        remote,
        daemon,
        repo,
    };
    wait_for_mux(
        &harness.home,
        &harness.remote,
        MuxState::Alive,
        "mux after setup",
    );
    harness
}

fn remote_docker(remote: &SshRemoteHost, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("docker");
    cmd.arg("exec").arg(&remote.container_id).arg("docker");
    cmd.args(args);
    cmd.success().context("running nested docker command")
}

fn inner_container(remote: &SshRemoteHost, pod_name: &str) -> String {
    let stdout = remote_docker(
        remote,
        &[
            "ps",
            "-q",
            "--filter",
            &format!("label=dev.rumpelpod.name={pod_name}"),
        ],
    )
    .expect("listing nested pod");
    let id = String::from_utf8_lossy(&stdout).trim().to_string();
    assert!(!id.is_empty(), "remote pod {pod_name} not found");
    id
}

fn commit_in_pod(remote: &SshRemoteHost, pod_name: &str, message: &str) -> String {
    let inner = inner_container(remote, pod_name);
    remote_docker(
        remote,
        &[
            "exec",
            &inner,
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
            TEST_REPO_PATH,
            "commit",
            "--allow-empty",
            "-m",
            message,
        ],
    )
    .expect("creating commit inside pod");
    let stdout = remote_docker(
        remote,
        &[
            "exec",
            &inner,
            "git",
            "-C",
            TEST_REPO_PATH,
            "rev-parse",
            "HEAD",
        ],
    )
    .expect("reading pod commit");
    String::from_utf8_lossy(&stdout).trim().to_string()
}

fn pod_ref(pod_name: &str) -> String {
    format!("refs/rumpelpod/{pod_name}@{pod_name}")
}

fn wait_for_ref(repo: &Path, git_ref: &str, expected: &str) {
    wait_for_refs(repo, &[(git_ref, expected)]);
}

fn wait_for_refs(repo: &Path, expected: &[(&str, &str)]) {
    let mut pending: Vec<(&str, &str)> = expected.to_vec();
    while !pending.is_empty() {
        pending.retain(|(git_ref, oid)| {
            let output = Command::new("git")
                .args(["rev-parse", git_ref])
                .current_dir(repo)
                .output()
                .expect("git rev-parse failed");
            !(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == *oid)
        });
        if pending.is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn kill_sshd_sessions(remote: &SshRemoteHost) {
    Command::new("docker")
        .args([
            "exec",
            &remote.container_id,
            "sh",
            "-c",
            "for pid in $(pgrep sshd); do [ \"$pid\" = 1 ] || kill -9 \"$pid\"; done",
        ])
        .success()
        .expect("killing sshd sessions");
}

fn kill_control_masters(home: &TestHome) {
    let sockets = home.path().join(".ssh/sockets");
    let pattern = sockets.display().to_string();
    let _ = Command::new("pkill").args(["-9", "-f", &pattern]).output();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MuxState {
    Alive,
    Dead,
}

fn mux_state(home: &TestHome, remote: &SshRemoteHost) -> MuxState {
    let config = home.path().join(".ssh/config");
    let output = Command::new("ssh")
        .args([
            "-F",
            &config.to_string_lossy(),
            "-O",
            "check",
            &remote.ssh_spec(),
        ])
        .output()
        .expect("ssh -O check");
    if output.status.success() {
        MuxState::Alive
    } else {
        MuxState::Dead
    }
}

/// Poll until `ssh -O check` reports `want`. The xtest timeout is the
/// only deadline; this does not send traffic to the remote. `what` is
/// logged while waiting so a hang names the wait that never finished.
pub(super) fn wait_for_mux(
    home: &TestHome,
    remote: &SshRemoteHost,
    want: MuxState,
    what: &str,
) -> MuxState {
    let started = Instant::now();
    let mut last_log = started;
    loop {
        let state = mux_state(home, remote);
        if state == want {
            return state;
        }
        if last_log.elapsed() >= Duration::from_secs(5) {
            let waited = started.elapsed().as_secs();
            eprintln!("waiting for {what}: want {want:?}, still {state:?} after {waited}s");
            last_log = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct ClientBlackhole {
    dest: String,
    port: u16,
}

impl ClientBlackhole {
    fn install(remote: &SshRemoteHost) -> Option<Self> {
        let (dest, port) = if let Some(port) = remote.published_port {
            ("127.0.0.1".to_string(), port)
        } else {
            (remote.ip_address.clone(), 22)
        };
        if run_iptables(&[
            "-I",
            "OUTPUT",
            "-d",
            &dest,
            "-p",
            "tcp",
            "--dport",
            &port.to_string(),
            "-j",
            "DROP",
        ]) {
            Some(Self { dest, port })
        } else {
            None
        }
    }
}

impl Drop for ClientBlackhole {
    fn drop(&mut self) {
        let port = self.port.to_string();
        let _ = run_iptables(&[
            "-D", "OUTPUT", "-d", &self.dest, "-p", "tcp", "--dport", &port, "-j", "DROP",
        ]);
    }
}

fn run_iptables(args: &[&str]) -> bool {
    let mut direct = Command::new("iptables");
    direct.args(["-w", "5"]);
    direct.args(args);
    if matches!(direct.status(), Ok(status) if status.success()) {
        return true;
    }
    let mut elevated = Command::new("sudo");
    elevated.args(["-n", "iptables", "-w", "5"]);
    elevated.args(args);
    matches!(elevated.status(), Ok(status) if status.success())
}

/// Server-side session death: sshd children are gone, listener stays.
#[test]
fn mux_git_syncs_after_sshd_sessions_killed() {
    if skip_if_unsupported() {
        return;
    }

    let harness = start_mux_harness(&["alpha", "beta"]);
    kill_sshd_sessions(&harness.remote);
    wait_for_mux(
        &harness.home,
        &harness.remote,
        MuxState::Dead,
        "mux after sshd kill",
    );
    let oid = commit_in_pod(&harness.remote, "alpha", "after-session-kill");
    wait_for_ref(harness.repo.path(), &pod_ref("alpha"), &oid);
}

/// Client-side blackhole: packets to the SSH port are dropped, like a
/// laptop whose WiFi is gone. ServerAlive should tear down the mux
/// master; after the route returns the daemon should recover alone.
#[test]
fn mux_git_syncs_after_client_blackhole() {
    if skip_if_unsupported() {
        return;
    }

    let harness = start_mux_harness(&["alpha", "beta"]);
    let Some(hole) = ClientBlackhole::install(&harness.remote) else {
        println!("xtest:skip");
        return;
    };
    wait_for_mux(
        &harness.home,
        &harness.remote,
        MuxState::Dead,
        "mux after client blackhole",
    );
    let oid = commit_in_pod(&harness.remote, "alpha", "after-blackhole");
    drop(hole);
    wait_for_ref(harness.repo.path(), &pod_ref("alpha"), &oid);
}

/// Two pods each have a pending commit while the client cannot reach
/// SSH. Both refs should land after the blackhole is lifted.
#[test]
fn mux_both_pods_sync_after_client_blackhole() {
    if skip_if_unsupported() {
        return;
    }

    let harness = start_mux_harness(&["alpha", "beta"]);
    let Some(hole) = ClientBlackhole::install(&harness.remote) else {
        println!("xtest:skip");
        return;
    };
    wait_for_mux(
        &harness.home,
        &harness.remote,
        MuxState::Dead,
        "mux after client blackhole",
    );
    let oid_a = commit_in_pod(&harness.remote, "alpha", "alpha-pending");
    let oid_b = commit_in_pod(&harness.remote, "beta", "beta-pending");
    drop(hole);
    wait_for_ref(harness.repo.path(), &pod_ref("alpha"), &oid_a);
    wait_for_ref(harness.repo.path(), &pod_ref("beta"), &oid_b);
}

/// The mux master process is killed and its socket is left behind.
#[test]
fn mux_git_syncs_after_control_master_killed() {
    if skip_if_unsupported() {
        return;
    }

    let harness = start_mux_harness(&["alpha", "beta"]);
    let before = mux_state(&harness.home, &harness.remote);
    kill_control_masters(&harness.home);
    let after = wait_for_mux(
        &harness.home,
        &harness.remote,
        MuxState::Dead,
        "mux after control master kill",
    );
    assert_ne!(
        before, after,
        "control master should not still be {before:?}"
    );
    let oid = commit_in_pod(&harness.remote, "alpha", "after-mux-kill");
    wait_for_ref(harness.repo.path(), &pod_ref("alpha"), &oid);
}

/// Every pod on the muxed host should be enterable after sshd sessions die.
#[test]
fn mux_all_pods_enter_after_sshd_sessions_killed() {
    if skip_if_unsupported() {
        return;
    }

    let pods = ["one", "two", "three"];
    let harness = start_mux_harness(&pods);
    kill_sshd_sessions(&harness.remote);
    wait_for_mux(
        &harness.home,
        &harness.remote,
        MuxState::Dead,
        "mux after sshd kill",
    );
    let ssh_config = harness.home.path().join(".ssh/config");
    harness.remote.wait_for_ssh_connectivity(&ssh_config);

    for name in pods {
        let stdout = pod_command(&harness.repo, &harness.daemon)
            .args(["enter", name, "--", "echo", "reconnected"])
            .success()
            .unwrap_or_else(|e| panic!("entering {name} after sshd kill failed: {e:#}"));
        assert_eq!(String::from_utf8_lossy(&stdout).trim(), "reconnected");
    }
}

/// Remote freeze: the host is paused long enough for client keepalives
/// to give up, then resumed. Same shape as a laptop sleep.
#[test]
fn mux_git_syncs_after_remote_pause() {
    if skip_if_unsupported() {
        return;
    }

    let harness = start_mux_harness(&["alpha", "beta"]);
    Command::new("docker")
        .args(["pause", &harness.remote.container_id])
        .success()
        .expect("pausing ssh host");
    // Nested dockerd is frozen; commit after unpause. Mux death is
    // OpenSSH's ServerAlive, which we poll for instead of sleeping.
    wait_for_mux(
        &harness.home,
        &harness.remote,
        MuxState::Dead,
        "mux while paused",
    );
    Command::new("docker")
        .args(["unpause", &harness.remote.container_id])
        .success()
        .expect("unpausing ssh host");
    let oid = commit_in_pod(&harness.remote, "alpha", "after-pause");
    wait_for_ref(harness.repo.path(), &pod_ref("alpha"), &oid);
}

/// Ten pods on one muxed host, then a client-side disconnect. Every
/// pending commit should land and every pod should be enterable
/// without an explicit `rumpel connect`.
#[test]
fn mux_ten_pods_reconnect_after_disconnect() {
    if skip_if_unsupported() {
        return;
    }

    let pods: Vec<String> = (0..10).map(|i| format!("load-{i:02}")).collect();
    let names: Vec<&str> = pods.iter().map(String::as_str).collect();
    let harness = start_mux_harness(&names);

    let hole = ClientBlackhole::install(&harness.remote);
    if hole.is_none() {
        kill_sshd_sessions(&harness.remote);
    }
    wait_for_mux(
        &harness.home,
        &harness.remote,
        MuxState::Dead,
        "mux after load disconnect",
    );

    let mut commits = Vec::new();
    for name in &pods {
        let oid = commit_in_pod(&harness.remote, name, "load-pending");
        commits.push((pod_ref(name), oid));
    }
    drop(hole);

    let expected: Vec<(&str, &str)> = commits
        .iter()
        .map(|(git_ref, oid)| (git_ref.as_str(), oid.as_str()))
        .collect();
    wait_for_refs(harness.repo.path(), &expected);

    let ssh_config = harness.home.path().join(".ssh/config");
    harness.remote.wait_for_ssh_connectivity(&ssh_config);
    for name in &pods {
        let stdout = pod_command(&harness.repo, &harness.daemon)
            .args(["enter", name, "--", "echo", "reconnected"])
            .success()
            .unwrap_or_else(|e| panic!("entering {name} after load reconnect failed: {e:#}"));
        assert_eq!(String::from_utf8_lossy(&stdout).trim(), "reconnected");
    }
}
