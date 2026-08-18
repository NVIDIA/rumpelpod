// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test that the rumpelpod system prompt is injected into
//! ~/.grok/rules/rumpelpod.md inside the container and visible to the
//! Grok agent.

use super::common::{setup_grok_test_repo, GrokSession};
use crate::common::pod_command;

#[test]
fn grok_system_prompt_is_written_and_discovered() {
    let (_home, repo, _executor, daemon) = setup_grok_test_repo();
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "grok-prompt-file",
            "--",
            "sh",
            "-c",
            "grep -F DESCRIPTION \"$HOME/.grok/rules/rumpelpod.md\" \
             && grep -F 'rumpelpod/' \"$HOME/.grok/rules/rumpelpod.md\" \
             && /opt/rumpelpod/bin/grok inspect --json",
        ])
        .output()
        .expect("rumpel enter failed");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "injected grok rules file should exist and grok inspect should run, \
         stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("rumpelpod.md"),
        "grok inspect should list the injected rules file, stdout: {stdout}"
    );
}

#[test]
fn grok_system_prompt_describes_remotes() {
    let (home, repo, _executor, daemon) = setup_grok_test_repo();

    let mut session = GrokSession::spawn(&repo, &daemon, home.path());

    session.wait_for("always-approve");
    // "all uppercase" prevents a false positive: the binary lives at
    // /opt/rumpelpod/bin/grok and that path can leak into the TUI.
    session.send("What git remote has other pods? One word, all uppercase.");
    session.wait_for("RUMPELPOD");
}

#[test]
fn grok_system_prompt_describes_description_file() {
    let (home, repo, _executor, daemon) = setup_grok_test_repo();

    let mut session = GrokSession::spawn(&repo, &daemon, home.path());

    session.wait_for("always-approve");
    session.send("In which file should you put the merge commit message? One word only.");
    session.wait_for("DESCRIPTION");
}
