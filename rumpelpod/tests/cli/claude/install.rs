// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Verify that `rumpel claude` auto-installs the Claude CLI when it
//! is not already present in the container image.

use std::fs;
use std::process::Command;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

use super::common::ClaudeSession;

/// Set up a container image with NO pre-installed Claude CLI.
///
/// The PTY handler's ensure_claude_cli will download it from GCS on
/// first session spawn.
fn setup_install_test_repo() -> (TestHome, TestRepo, ExecutorResources, TestDaemon) {
    let repo = TestRepo::new();

    // No extra Dockerfile steps -- the image has no claude binary.
    let extra_json = r#",
        "remoteEnv": {
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:${containerEnv:RUMPELPOD_SERVER_PORT}/llm-cache-proxy/anthropic"
        }"#;
    write_test_devcontainer(&repo, "", extra_json);

    let home = TestHome::new();
    super::common::setup_controlled_home(&home);
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start_with_local_llm_clis(&home);
    std::fs::write(repo.path().join(".rumpelpod.json"), &executor.json)
        .expect("write .rumpelpod.json");
    (home, repo, executor, daemon)
}

#[test]
fn claude_auto_install() {
    let (home, repo, _executor, daemon) = setup_install_test_repo();

    let mut session = ClaudeSession::spawn(&repo, &daemon, home.path(), "claude-haiku-4-5", &[]);

    // The PTY handler downloads the Claude CLI from GCS before
    // spawning the session.  Wait for the TUI to finish loading.
    session.wait_for("~/workspace");
}

/// The client resolves the claude binary path and sends it to the
/// daemon, so the prepared image includes Claude CLI even though the
/// daemon itself cannot find the binary on its own PATH.
///
/// We seed a client-only bin dir with `claude` and prepend it to
/// the client's PATH, while leaving the daemon's bin dir untouched.
/// This deliberately recreates the asymmetry `find_local_claude_cli`
/// was built to handle.
#[test]
fn image_includes_claude_from_client_path() {
    assert!(
        Command::new("claude")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success()),
        "claude must be in PATH to run this test",
    );

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    // Daemon's bin dir does not contain claude: verifies the daemon
    // cannot detect the local machine's CLI on its own.
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Give only the client a view of the local machine's `claude` so
    // find_local_claude_cli picks it up but the daemon does not.
    let client_only = home.client_only_bin_dir(&["claude"]);
    let client_path = format!("{}:{}", client_only.display(), daemon.bin_dir.display());

    let output = pod_command(&repo, &daemon)
        .env("PATH", client_path)
        .args([
            "enter",
            "--create",
            "claude-install-test",
            "--",
            "test",
            "-x",
            "/opt/rumpelpod/bin/claude",
        ])
        .output()
        .expect("rumpel enter failed");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "claude binary should exist in the container, stderr: {stderr}",
    );
}
