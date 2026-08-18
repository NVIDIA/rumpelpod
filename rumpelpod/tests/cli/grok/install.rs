// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Verify that `rumpel grok` installs the Grok CLI into the prepared
//! image based on the binary the client resolves on the local machine,
//! not the one the daemon happens to find on its own PATH.

use std::fs;
use std::process::Command;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;
use rumpelpod::CommandExt;

/// The client resolves the grok binary path and sends it to the
/// daemon, so the prepared image includes Grok CLI even though the
/// daemon itself cannot find the binary on its own PATH.
///
/// The installed binary must have the same version as the client-resolved
/// binary; selecting a rumpelpod-pinned release would make prepared images
/// drift from the host CLI the user is already running.
#[test]
fn image_includes_grok_from_client_path() {
    let host_output = Command::new("grok")
        .arg("--version")
        .output()
        .expect("grok must be in PATH to run this test");
    assert!(host_output.status.success(), "host grok --version failed");
    let host_version = String::from_utf8(host_output.stdout)
        .expect("host grok version should be UTF-8")
        .trim()
        .to_string();

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    // Daemon's bin dir does not contain grok: verifies the daemon
    // cannot detect the local machine's CLI on its own.
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Give only the client a view of the local machine's `grok` so
    // find_local_grok_cli picks it up but the daemon does not.
    let client_only = home.client_only_bin_dir(&["grok"]);
    let client_path = format!("{}:{}", client_only.display(), daemon.bin_dir.display());

    let stdout = pod_command(&repo, &daemon)
        .env("PATH", client_path)
        .args([
            "enter",
            "--create",
            "grok-install-test",
            "--",
            "/opt/rumpelpod/bin/grok",
            "--version",
        ])
        .success()
        .expect("grok binary should run in the container");
    let pod_version = String::from_utf8(stdout)
        .expect("pod grok version should be UTF-8")
        .trim()
        .to_string();
    // Host `grok --version` appends a channel tag (`[stable]`); the
    // versioned x.ai download of the same release does not. Compare the
    // release number we actually pin, not the full banner.
    assert_eq!(
        grok_release_version(&pod_version),
        grok_release_version(&host_version),
        "pod Grok version should match the host version (pod={pod_version:?}, host={host_version:?})",
    );
}

fn grok_release_version(raw: &str) -> &str {
    raw.split_whitespace()
        .nth(1)
        .expect("grok --version should include a version token")
}
