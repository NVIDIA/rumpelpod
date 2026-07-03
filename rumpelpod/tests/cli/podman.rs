// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Default Podman smoke tests.
//!
//! The executor-agnostic suite can be redirected to Podman with
//! `cargo xtest --executor podman` or `RUMPELPOD_TEST_EXECUTOR=podman`.
//! These tests run in the normal Docker-oriented suite and verify that
//! a host with Podman but no Docker still gets a working local executor,
//! and that a remote Podman host is reachable over SSH.

use std::fs;

use rumpelpod::CommandExt;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::{docker_available, skip_test, skip_unless_podman_executor};
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
    println!("xtest:timeout=300");
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
    // Remote image build plus base image pull on the nested Podman can
    // exceed the default timeout.
    println!("xtest:timeout=600");
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
