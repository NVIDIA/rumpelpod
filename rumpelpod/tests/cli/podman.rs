// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Default Podman smoke tests.
//!
//! The executor-agnostic suite can be redirected to Podman with
//! `cargo xtest --executor podman` or `RUMPELPOD_TEST_EXECUTOR=podman`.
//! These tests run in the normal Docker-oriented suite and verify that
//! a host with Podman but no Docker still gets a working local executor.

use std::fs;

use rumpelpod::CommandExt;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::skip_unless_podman_executor;

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
