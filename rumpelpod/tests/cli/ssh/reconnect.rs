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

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indoc::formatdoc;

use super::{write_ssh_config, SshRemoteHost};
use crate::codex::common::{setup_controlled_home, CodexSession};
use crate::common::{
    assert_process_exited, pod_command, wait_for_pid, write_test_devcontainer, TestDaemon,
    TestHome, TestProcess, TestRepo, TEST_REPO_PATH,
};
use crate::executor;
use rumpelpod::daemon::protocol::{Daemon, DaemonClient, PodListMode, PodStatus};
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
    start_mux_harness_with_env(pod_names, &[])
}

fn start_mux_harness_with_env(pod_names: &[&str], extra_env: &[(&str, &str)]) -> MuxHarness {
    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_mux_ssh_config(&home, &remote);
    let mut env = vec![("RUMPELPOD_HOST_PROBE_TIMEOUT_MS", FAST_PROBE_MS)];
    env.extend_from_slice(extra_env);
    let daemon = TestDaemon::start_with_env(&home, &env);

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

#[test]
fn mux_recovery_bounds_event_and_git_handshakes_together() {
    if skip_if_unsupported() {
        return;
    }
    let pods = ["one", "two", "three", "four"];
    let harness = start_mux_harness(&pods);
    harness.home.link_local_bins(&["rmdir"]);
    let Some(hole) = ClientBlackhole::install(&harness.remote) else {
        executor::skip_test();
        return;
    };
    TestProcess::spawn(pod_command(&harness.repo, &harness.daemon).args(["connect", "one"]))
        .failure(Duration::from_secs(10), "host probe must report outage");
    let client = DaemonClient::new_unix(&harness.daemon.socket_path);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = client
            .list_pods(
                harness.repo.path().to_path_buf(),
                PodListMode::Offline,
                false,
            )
            .unwrap();
        if status
            .iter()
            .all(|pod| pod.status == PodStatus::Disconnected)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pods still connected: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let docker = harness.home.bin_dir().join("docker");
    let real_docker = fs::canonicalize(&docker).unwrap();
    let fault = harness.home.path().join("handshakes");
    fs::create_dir(&fault).unwrap();
    fs::remove_file(&docker).unwrap();
    // Both paths must share admission. A limit on each independently still
    // overloads the host when event streams and git tunnels recover together.
    fs::write(
        &docker,
        formatdoc! {r#"
            #!/bin/sh
            set -eu
            case " $* " in
                *" tcp-proxy "*|*" tunnel-server "*)
                    slot=''
                    for candidate in 1 2; do
                        if mkdir '{fault}/slot-'"$candidate" 2>/dev/null; then
                            slot='{fault}/slot-'"$candidate"
                            break
                        fi
                    done
                    if [ -z "$slot" ]; then
                        touch '{fault}/overflow'
                    fi
                    printf '%s\n' "$*" >> '{fault}/attempts'
                    sleep 2
                    if [ -n "$slot" ]; then
                        rmdir "$slot"
                    fi
                    ;;
            esac
            exec '{real_docker}' "$@"
        "#, fault = fault.display(), real_docker = real_docker.display()},
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();

    let commits: Vec<_> = pods
        .iter()
        .map(|name| {
            (
                pod_ref(name),
                commit_in_pod(&harness.remote, name, "bounded-recovery"),
            )
        })
        .collect();
    drop(hole);
    let expected: Vec<_> = commits
        .iter()
        .map(|(git_ref, oid)| (git_ref.as_str(), oid.as_str()))
        .collect();
    wait_for_refs(harness.repo.path(), &expected);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = client
            .list_pods(
                harness.repo.path().to_path_buf(),
                PodListMode::Offline,
                false,
            )
            .unwrap();
        if status.iter().all(|pod| pod.status == PodStatus::Running) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "event streams did not recover: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let attempts = fs::read_to_string(fault.join("attempts")).unwrap();
    assert!(
        attempts.matches("tcp-proxy").count() >= pods.len(),
        "{attempts}"
    );
    assert!(
        attempts.matches("tunnel-server").count() >= pods.len(),
        "{attempts}"
    );
    assert!(
        !fault.join("overflow").exists(),
        "too many simultaneous handshakes:\n{attempts}"
    );
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

#[test]
fn codex_frontend_recovers_after_ssh_retry_exhaustion() {
    check_codex_frontend_recovery(false);
}

#[test]
fn codex_frontend_recovers_after_ssh_retry_exhaustion_while_detached() {
    check_codex_frontend_recovery(true);
}

fn check_codex_frontend_recovery(detach_before_recovery: bool) {
    if skip_if_unsupported() {
        return;
    }
    // The banner is a compatibility boundary. Exercise the real version
    // installed for tests by the devcontainer, not a stub that prints it.
    let expected_version = include_str!("../../../../.devcontainer/Dockerfile")
        .lines()
        .find_map(|line| line.strip_prefix("ARG CODEX_VERSION="))
        .expect("devcontainer pins the supported Codex version");
    let version = Command::new("codex")
        .arg("--version")
        .success()
        .expect("read test Codex version");
    assert_eq!(
        String::from_utf8_lossy(&version).trim(),
        format!("codex-cli {expected_version}")
    );

    let home = TestHome::new();
    setup_controlled_home(&home);
    home.link_local_bins(&["codex"]);
    let remote = SshRemoteHost::start();
    write_mux_ssh_config(&home, &remote);
    let daemon =
        TestDaemon::start_with_env(&home, &[("RUMPELPOD_HOST_PROBE_TIMEOUT_MS", FAST_PROBE_MS)]);
    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let config = serde_json::to_string(&serde_json::json!({"host": remote.ssh_spec()})).unwrap();
    std::fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.wait_for_ready_with_timeout(Duration::from_secs(90));
    session.send("What is the capital of France? Reply with just the city name, nothing else.");
    session.wait_for_with_timeout("Paris", Duration::from_secs(45));
    let original_pid = codex_frontend_pids(&daemon);
    assert_eq!(original_pid.len(), 1);
    // A healthy composer containing the trigger must not restart the frontend.
    let draft = "Reconnect failed in an unsent draft";
    session.write_raw(draft.as_bytes());
    session.wait_for_with_timeout(draft, Duration::from_secs(10));
    wait_for_mux(&home, &remote, MuxState::Alive, "mux before Codex outage");

    // A short outage is the control: the same frontend can resume its thread
    // and preserve a draft when transport recovery beats Codex's deadline.
    let paused = PausedCodexHost::new(&remote);
    wait_for_mux(&home, &remote, MuxState::Dead, "mux during short outage");
    session.wait_for_with_timeout("Reconnecting to server", Duration::from_secs(30));
    paused.resume();
    session.wait_for_absent_with_timeout("Reconnecting to server", Duration::from_secs(60));
    session.wait_for_with_timeout(draft, Duration::from_secs(15));
    assert_eq!(codex_frontend_pids(&daemon), original_pid);
    eprintln!("The original Codex frontend recovered from the short SSH outage");
    wait_for_mux(&home, &remote, MuxState::Alive, "mux before long outage");

    let outage = Instant::now();
    let paused = PausedCodexHost::new(&remote);
    wait_for_mux(&home, &remote, MuxState::Dead, "mux during long outage");
    session.wait_for_with_timeout("Reconnecting to server", Duration::from_secs(30));
    session.wait_for_with_timeout(
        "Automatic reconnect could not restore",
        Duration::from_secs(150),
    );
    assert!(
        outage.elapsed() >= Duration::from_secs(110),
        "Codex failed before its retry budget could have elapsed"
    );
    eprintln!(
        "Codex gave up after {:?} with the frontend still alive",
        outage.elapsed()
    );
    assert!(session.screen().contents().contains(draft));
    assert_eq!(codex_frontend_pids(&daemon), original_pid);
    if detach_before_recovery {
        session.write_raw(&[0x01, b'd']);
        session.wait_for_exit();
    }

    paused.resume();
    let oid = commit_in_pod(
        &remote,
        "test",
        "prove transport recovered after Codex gave up",
    );
    wait_for_ref(repo.path(), &pod_ref("test"), &oid);
    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "test", "--", "echo", "transport-recovered"])
        .success()
        .expect("enter pod after SSH recovery");
    assert_eq!(
        String::from_utf8_lossy(&stdout).trim(),
        "transport-recovered"
    );

    // Observe replacement before attaching again: detached sessions must
    // recover without an incoming client driving their lifecycle.
    let deadline = Instant::now() + Duration::from_secs(45);
    let replacement_pid = loop {
        let pids = codex_frontend_pids(&daemon);
        if pids.len() == 1 && pids != original_pid {
            break pids[0];
        }
        assert!(
            Instant::now() < deadline,
            "exhausted Codex frontend was not replaced"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let command = Command::new("ps")
        .args(["-ww", "-p", &replacement_pid.to_string(), "-o", "args="])
        .success()
        .expect("read replacement frontend command");
    let command = String::from_utf8(command).expect("frontend command is UTF-8");
    let args: Vec<_> = command.split_whitespace().collect();
    assert!(
        args.windows(2).any(|args| args == ["resume", "--last"]),
        "recovery should let Codex choose the last thread: {command}"
    );
    if detach_before_recovery {
        session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    }
    session.wait_for_absent_with_timeout(draft, Duration::from_secs(30));
    session.wait_for_with_timeout("Paris", Duration::from_secs(30));
    session.send_with_timeout("/status", Duration::from_secs(10));
    session.wait_for_with_timeout("Permissions:", Duration::from_secs(15));
    // A real new turn proves that input was moved to the replacement PTY and
    // the frontend resumed the remote session, rather than replaying a screen.
    session.send_with_timeout("Run `printf %x 3735928559`.", Duration::from_secs(10));
    session.wait_for_with_timeout("deadbeef", Duration::from_secs(45));
    session.send_with_timeout("/exit", Duration::from_secs(10));
    session.wait_for_exit();
    assert!(codex_frontend_pids(&daemon).is_empty());
}

fn codex_frontend_pids(daemon: &TestDaemon) -> Vec<u32> {
    let binary = daemon.bin_dir.join("codex");
    let binary = binary.to_str().expect("test binary path is UTF-8");
    let processes = Command::new("ps")
        .args(["-eo", "pid=,args="])
        .success()
        .expect("list local frontend processes");
    String::from_utf8(processes)
        .expect("process listing is UTF-8")
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?;
            if fields.next() == Some(binary) && fields.next() == Some("--remote") {
                Some(pid.parse().expect("frontend PID"))
            } else {
                None
            }
        })
        .collect()
}

// Restore connectivity before TestDaemon's cleanup contacts the remote host,
// including when a diagnostic assertion panics during an outage.
struct PausedCodexHost<'a> {
    remote: &'a SshRemoteHost,
    paused: bool,
}

impl<'a> PausedCodexHost<'a> {
    fn new(remote: &'a SshRemoteHost) -> Self {
        Command::new("docker")
            .args(["pause", &remote.container_id])
            .success()
            .expect("pause SSH host");
        Self {
            remote,
            paused: true,
        }
    }

    fn resume(mut self) {
        Command::new("docker")
            .args(["unpause", &self.remote.container_id])
            .success()
            .expect("restore SSH host");
        self.paused = false;
    }
}

impl Drop for PausedCodexHost<'_> {
    fn drop(&mut self) {
        if self.paused {
            if let Err(error) = Command::new("docker")
                .args(["unpause", &self.remote.container_id])
                .success()
            {
                eprintln!("restoring SSH host after failed Codex diagnostic: {error:#}");
            }
        }
    }
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

// A successful host probe cannot prove that an older Docker client has
// finished. Delay completion of one real inspection while other SSH sessions
// recover, including the host probe and a second pod's connection.
#[test]
fn mux_stalled_inspection_releases_lifecycle_lock_after_recovery() {
    if skip_if_unsupported() {
        return;
    }
    let harness = start_mux_harness_with_env(
        &["alpha", "beta"],
        &[("RUMPELPOD_CONTROL_TIMEOUT_MS", "10000")],
    );
    let docker = harness.home.bin_dir().join("docker");
    let real_docker = fs::canonicalize(&docker).expect("resolve docker binary");
    let fault = harness.home.path().join("control-fault");
    fs::create_dir(&fault).unwrap();
    fs::remove_file(&docker).unwrap();
    fs::write(
        &docker,
        formatdoc! {r#"
            #!/bin/sh
            set -eu
            case " $* " in
                *" container inspect "*)
                    if [ -f '{fault}/armed' ] && mkdir '{fault}/claimed' 2>/dev/null; then
                        '{real_docker}' "$@"
                        echo $$ > '{fault}/parent'
                        sh -c 'echo $$ > "{fault}/child"; exec sleep 90' &
                        wait
                        exit 0
                    fi
                    ;;
            esac
            exec '{real_docker}' "$@"
        "#,
            fault = fault.display(),
            real_docker = real_docker.display(),
        },
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();

    for (round, name) in ["alpha", "beta", "alpha"].into_iter().enumerate() {
        fs::write(fault.join("armed"), "").unwrap();
        // Use the launch API directly: a healthy CLI can take the prepared
        // connection fast path without asking the daemon to inspect anything.
        let body = serde_json::json!({
            "pod_name": name,
            "repo_path": harness.repo.path(),
            "host": rumpelpod::config::Host::parse(&harness.remote.ssh_spec()).unwrap(),
            "devcontainer_path": null, "host_branch": null, "git_identity": null,
            "claude_cli_path": null, "codex_cli_path": null,
            "pi_cli_path": null, "grok_cli_path": null,
            "inject_system_prompt": false, "description_file": null,
            "local_env_vars": {}, "client_env": {},
            "client_context": {"ssh_auth_sock": null},
        })
        .to_string();
        let request = || {
            let mut command = Command::new("curl");
            command
                .args([
                    "--noproxy",
                    "*",
                    "--silent",
                    "--show-error",
                    "--no-buffer",
                    "--fail",
                ])
                .arg("--unix-socket")
                .arg(&harness.daemon.socket_path)
                .args([
                    "-X",
                    "PUT",
                    "-H",
                    "Content-Type: application/json",
                    "--data-binary",
                    &body,
                    "http://localhost/pod",
                ]);
            command
        };
        let mut first = TestProcess::spawn(&mut request());
        let deadline = Instant::now() + Duration::from_secs(15);
        while !fault.join("parent").exists() {
            assert!(
                Instant::now() < deadline,
                "inspection did not stall: {}",
                first.output()
            );
            assert!(
                !first.output().contains("event: error"),
                "{}",
                first.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let parent = wait_for_pid(&fault.join("parent"));
        let child = wait_for_pid(&fault.join("child"));
        if round != 1 {
            first.abandon();
        }
        let mut queued = TestProcess::spawn(&mut request());
        queued.wait_for_output("waiting for another operation on this pod");

        if round != 1 {
            let hole = ClientBlackhole::install(&harness.remote)
                .expect("regression needs packet filtering for its disposable SSH host");
            let output = TestProcess::spawn(
                pod_command(&harness.repo, &harness.daemon).args(["connect", "beta"]),
            )
            .failure(Duration::from_secs(10), "host outage must be detected");
            assert!(
                output.contains("SSH docker transport")
                    || output.contains("waiting for another host probe"),
                "{output}"
            );
            let client = DaemonClient::new_unix(&harness.daemon.socket_path);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let pods = client
                    .list_pods(
                        harness.repo.path().to_path_buf(),
                        PodListMode::Offline,
                        false,
                    )
                    .unwrap();
                if pods.iter().all(|pod| pod.status == PodStatus::Disconnected) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "host loss did not invalidate both pods: {pods:?}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(hole);
            let ssh_config = harness.home.path().join(".ssh/config");
            harness.remote.wait_for_ssh_connectivity(&ssh_config);
            TestProcess::spawn(
                pod_command(&harness.repo, &harness.daemon).args(["connect", "beta"]),
            )
            .success(
                Duration::from_secs(20),
                "other pod must recover independently",
            );
        }

        if round == 1 {
            let output = first.success(Duration::from_secs(20), "stalled inspection must expire");
            assert!(
                output.contains("container inspection timed out"),
                "{output}"
            );
        }
        let output = queued.success(
            Duration::from_secs(20),
            "host recovery must not leave entry queued behind a stale inspection",
        );
        assert!(output.contains("event: result"), "{output}");
        assert_process_exited(parent);
        assert_process_exited(child);
        TestProcess::spawn(
            pod_command(&harness.repo, &harness.daemon).args(["enter", name, "--", "true"]),
        )
        .success(Duration::from_secs(20), "fresh entry after cancellation");
        assert_eq!(mux_state(&harness.home, &harness.remote), MuxState::Alive);
        fs::remove_file(fault.join("armed")).unwrap();
        fs::remove_dir(fault.join("claimed")).unwrap();
        fs::remove_file(fault.join("parent")).unwrap();
        fs::remove_file(fault.join("child")).unwrap();
    }
}

#[test]
fn mux_stalled_tunnel_setup_allows_a_later_connection() {
    if skip_if_unsupported() {
        return;
    }
    let harness = start_mux_harness_with_env(
        &["tunnel-stall"],
        &[("RUMPELPOD_CONTROL_TIMEOUT_MS", "10000")],
    );
    let docker = harness.home.bin_dir().join("docker");
    let real_docker = fs::canonicalize(&docker).unwrap();
    let armed = harness.home.path().join("stall-tunnel");
    fs::write(&armed, "").unwrap();
    fs::remove_file(&docker).unwrap();
    fs::write(
        &docker,
        formatdoc! {r#"
        #!/bin/sh
        set -eu
        case " $* " in
            *" tunnel-server "*|*" tunnel-server")
                if [ -f '{armed}' ]; then
                    echo $$ > '{armed}.pid'
                    exec sleep 90
                fi
                ;;
        esac
        exec '{real_docker}' "$@"
    "#, armed = armed.display(), real_docker = real_docker.display()},
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();

    // Kill only this fixture's tunnel server. Background repair and the
    // foreground connection can then contend on the same setup mutex.
    let container = inner_container(&harness.remote, "tunnel-stall");
    remote_docker(
        &harness.remote,
        &[
            "exec",
            &container,
            "pkill",
            "-f",
            "^/opt/rumpelpod/bin/rumpel tunnel-server$",
        ],
    )
    .expect("stop disposable tunnel server");
    let stalled = wait_for_pid(&armed.with_extension("pid"));
    let mut connect = TestProcess::spawn(
        pod_command(&harness.repo, &harness.daemon).args(["connect", "tunnel-stall"]),
    );
    let output = connect.failure(Duration::from_secs(35), "tunnel setup must have a deadline");
    assert!(output.contains("git tunnel setup"), "{output}");
    fs::remove_file(armed).unwrap();
    TestProcess::spawn(
        pod_command(&harness.repo, &harness.daemon).args(["connect", "tunnel-stall"]),
    )
    .success(
        Duration::from_secs(30),
        "repaired transport must connect without stale setup work",
    );
    assert_process_exited(stalled);
    TestProcess::spawn(pod_command(&harness.repo, &harness.daemon).args([
        "enter",
        "tunnel-stall",
        "--",
        "echo",
        "connected",
    ]))
    .success(
        Duration::from_secs(20),
        "entry must work after tunnel repair",
    );
}

#[test]
fn mux_pod_proxy_reaps_stalled_sessions_and_preserves_listener() {
    if skip_if_unsupported() {
        return;
    }
    let harness = start_mux_harness(&["proxy-stall"]);
    let client = DaemonClient::new_unix(&harness.daemon.socket_path);
    let endpoint = client
        .prepared_pod_connection(rumpelpod::daemon::protocol::ConnectPodRequest {
            pod_name: "proxy-stall".into(),
            repo_path: harness.repo.path().to_path_buf(),
            client_context: Default::default(),
        })
        .unwrap()
        .unwrap();
    let docker = harness.home.bin_dir().join("docker");
    let real_docker = fs::canonicalize(&docker).unwrap();
    let fault = harness.home.path().join("proxy-fault");
    fs::create_dir(&fault).unwrap();
    fs::remove_file(&docker).unwrap();
    fs::write(
        &docker,
        formatdoc! {r#"
        #!/bin/sh
        set -eu
        case " $* " in
            *" tcp-proxy "*)
                if mkdir '{fault}/claimed' 2>/dev/null; then
                    echo $$ > '{fault}/pid'
                    exec sleep 90
                fi
                ;;
        esac
        exec '{real_docker}' "$@"
    "#, fault = fault.display(), real_docker = real_docker.display()},
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    let request = || {
        let mut command = Command::new("curl");
        command
            .args(["--noproxy", "*", "--silent", "--show-error", "--fail"])
            .arg("-H")
            .arg(format!(
                "Authorization: Bearer {}",
                endpoint.container_token
            ))
            .arg(format!("{}/state", endpoint.container_url));
        command
    };

    for host_loss in [false, true] {
        let mut pending = TestProcess::spawn(&mut request());
        let stalled = wait_for_pid(&fault.join("pid"));
        if host_loss {
            let hole = ClientBlackhole::install(&harness.remote)
                .expect("regression needs packet filtering for its disposable SSH host");
            TestProcess::spawn(
                pod_command(&harness.repo, &harness.daemon).args(["connect", "proxy-stall"]),
            )
            .failure(Duration::from_secs(10), "host probe must report outage");
            assert_process_exited(stalled);
            pending.failure(
                Duration::from_secs(5),
                "host reset must close the stalled HTTP session",
            );
            drop(hole);
            harness
                .remote
                .wait_for_ssh_connectivity(&harness.home.path().join(".ssh/config"));
        } else {
            // No host transition: cancelling a timed-out HTTP observer alone
            // must reap its exec instead of waiting forever on remote stdout.
            pending.abandon();
            assert_process_exited(stalled);
        }
        // Reuse the original URL; resetting a bridge must not replace its
        // listener or interrupt an independently established SSH master.
        TestProcess::spawn(&mut request()).success(
            Duration::from_secs(15),
            "original pod endpoint must still work",
        );
        if !host_loss {
            fs::remove_file(fault.join("pid")).unwrap();
            fs::remove_dir(fault.join("claimed")).unwrap();
        }
    }
}
