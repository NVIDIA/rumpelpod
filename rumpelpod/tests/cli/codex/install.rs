// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Verify that `rumpel codex` installs the Codex CLI into the prepared
//! image based on the binary the client resolves on the local machine,
//! not the one the daemon happens to find on its own PATH.

use std::fs;
use std::process::Command;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;
use rumpelpod::CommandExt;

/// The client resolves the codex binary path and sends it to the
/// daemon, so the prepared image includes Codex CLI even though the
/// daemon itself cannot find the binary on its own PATH.
///
/// The installed binary must have the same version as the client-resolved
/// binary; selecting the moving `latest` release would make prepared images
/// depend on when and where they were built.
#[test]
fn image_includes_codex_from_client_path() {
    let host_output = Command::new("codex")
        .arg("--version")
        .output()
        .expect("codex must be in PATH to run this test");
    assert!(host_output.status.success(), "host codex --version failed");
    let host_version = String::from_utf8(host_output.stdout)
        .expect("host codex version should be UTF-8")
        .trim()
        .to_string();

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    // Daemon's bin dir does not contain codex: verifies the daemon
    // cannot detect the local machine's CLI on its own.
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Give only the client a view of the local machine's `codex` so
    // find_local_codex_cli picks it up but the daemon does not.
    let client_only = home.client_only_bin_dir(&["codex"]);
    let client_path = format!("{}:{}", client_only.display(), daemon.bin_dir.display());

    let stdout = pod_command(&repo, &daemon)
        .env("PATH", client_path)
        .args([
            "enter",
            "--create",
            "codex-install-test",
            "--",
            "/opt/rumpelpod/bin/codex",
            "--version",
        ])
        .success()
        .expect("codex binary should run in the container");
    let pod_version = String::from_utf8(stdout)
        .expect("pod codex version should be UTF-8")
        .trim()
        .to_string();
    assert_eq!(
        pod_version, host_version,
        "pod Codex version should match the host version",
    );
}
