// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Codex conversation recovery when the host-side TUI must be relaunched.

use std::time::Duration;

use super::common::{setup_codex_test_repo, CodexSession};

#[test]
fn codex_relaunch_resumes_last_remote_session() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();

    // The fresh pod has no thread, so this also verifies that resume --last
    // falls through to creating one rather than rejecting the first launch.
    let mut first = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    first.dismiss_dialogs();
    first.send("What is the capital of France? Reply with just the city name, nothing else.");
    first.wait_for("Paris");

    // Exiting the local frontend leaves the per-pod app-server and its thread
    // alive, which is the failure boundary a replacement frontend must recover.
    first.send("/exit");
    first.wait_for_exit();

    let mut resumed = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    resumed.wait_for_with_timeout("Paris", Duration::from_secs(30));
    assert!(resumed.screen().contents().contains("YOLO mode"));
    resumed.send("Run `printf %x 3735928559`.");
    resumed.wait_for_with_timeout("deadbeef", Duration::from_secs(30));
}
