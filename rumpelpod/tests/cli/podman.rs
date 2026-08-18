// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Default Podman smoke tests.
//!
//! The executor-agnostic suite can be redirected to Podman with
//! `cargo xtest --executor podman` or `RUMPELPOD_TEST_EXECUTOR=podman`.
//! These tests run in the normal Docker-oriented suite and verify that
//! a host with Podman but no Docker still gets a working local executor,
//! and that a remote Podman host is reachable over SSH.
//!
//! Also covers `ContainerEngine::Auto` resolution for pods stored by
//! pre-Podman rumpelpod versions, which never had a per-pod engine to
//! resolve in the first place.

use std::fs;

use rumpelpod::CommandExt;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::{
    self, docker_available, skip_test, skip_unless_podman_executor, ExecutorResources,
};
use crate::ssh::{write_ssh_config, SshRemoteHost};

fn last_stdout_line(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .lines()
        .last()
        .unwrap_or("")
        .to_string()
}

#[test]
fn podman_auto_fallback_and_explicit_engine_smoke() {
    if !skip_unless_podman_executor() {
        return;
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    home.link_local_bin("podman");
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "--create", "auto-podman", "--", "echo", "auto"])
        .success()
        .expect("rumpel enter with auto Podman fallback failed");
    assert_eq!(last_stdout_line(&stdout), "auto");

    fs::write(
        repo.path().join(".rumpelpod.json"),
        r#"{ "containerEngine": "podman" }"#,
    )
    .unwrap();
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "explicit-podman",
            "--",
            "echo",
            "explicit",
        ])
        .success()
        .expect("rumpel enter with explicit Podman engine failed");
    assert_eq!(last_stdout_line(&stdout), "explicit");

    write_test_devcontainer(&repo, "", r#","runArgs": ["--runtime=runc"]"#);
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "podman-runtime",
            "--",
            "echo",
            "runtime",
        ])
        .output()
        .expect("failed to run rumpel enter");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "rumpel enter should ignore Podman's unsupported --runtime runArg.\nstderr: {stderr}"
    );
    assert_eq!(last_stdout_line(&output.stdout), "runtime");
    assert!(
        stderr.contains("warning: --runtime in runArgs is ignored by Podman"),
        "client stderr should warn that Podman ignores --runtime.\nstderr: {stderr}"
    );
}

#[test]
fn podman_ssh_smoke() {
    // The daemon side needs a local podman client; the fixture needs
    // local Docker to host the remote container.
    if !skip_unless_podman_executor() {
        return;
    }
    if !docker_available() {
        skip_test();
        return;
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    let remote = SshRemoteHost::start_podman();
    write_ssh_config(&home, &[&remote]);
    home.link_local_bin("podman");
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    let host = remote.ssh_spec();
    fs::write(
        repo.path().join(".rumpelpod.json"),
        format!(r#"{{ "host": "{host}", "containerEngine": "podman" }}"#),
    )
    .unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "--create", "podman-ssh", "--", "echo", "remote"])
        .success()
        .expect("rumpel enter on podman ssh host failed");
    assert_eq!(last_stdout_line(&stdout), "remote");

    // Re-enter reconnects to the existing remote pod through the
    // daemon's proxy instead of creating a new one.
    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "--create", "podman-ssh", "--", "echo", "again"])
        .success()
        .expect("rumpel re-enter on podman ssh host failed");
    assert_eq!(last_stdout_line(&stdout), "again");
}

/// Pre-Podman versions stored `Host::Localhost` as the bare JSON string
/// `"Localhost"` rather than today's struct variant. It still
/// deserializes fine (see the `Host` custom `Deserialize` impl), but
/// carries `ContainerEngine::Auto` forever since nothing re-resolves a
/// loaded `Host` before the daemon reconnects to running pods on
/// startup. Restoring such a pod used to panic in `Executor::new` and
/// crash-loop the whole daemon.
#[test]
fn restart_resolves_legacy_auto_engine_host() {
    // Restore-on-startup only exercises the localhost Docker path.
    if !matches!(executor::executor_mode(), executor::ExecutorMode::Docker) {
        executor::skip_test();
        return;
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let mut daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "legacy-host-pod";

    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("initial enter failed");

    daemon.kill();

    let db_path = home.path().join("state/rumpelpod/db.sqlite");
    let conn = rusqlite::Connection::open(&db_path).expect("opening test db");
    let updated = conn
        .execute(
            "UPDATE pods SET host = '\"Localhost\"' WHERE name = ?1",
            rusqlite::params![pod_name],
        )
        .expect("rewriting stored host to the legacy format");
    assert_eq!(updated, 1, "expected to rewrite exactly one pod row");
    drop(conn);

    // Restarting the daemon must survive restoring this pod instead of
    // panicking on the unresolved Auto engine.
    let daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["list"])
        .success()
        .expect("daemon should stay up after restoring a legacy-format pod");
}
