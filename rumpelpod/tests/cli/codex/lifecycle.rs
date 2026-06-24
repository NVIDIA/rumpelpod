// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Codex session behavior across pod lifecycle changes.

use std::time::Duration;

use super::common::{setup_codex_test_repo, CodexSession};
use crate::common::pod_command;
use rumpelpod::CommandExt;

#[test]
fn codex_reconnects_after_delete_recreate_same_name() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();

    {
        let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
        session.dismiss_dialogs();
        session.send("What is the capital of France? Reply with just the city name, nothing else.");
        session.wait_for("Paris");
    }

    pod_command(&repo, &daemon)
        .args(["delete", "--wait", "--force", "test"])
        .success()
        .expect("delete should remove the first pod");

    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.dismiss_dialogs_with_timeout(Duration::from_secs(30));
    session.send_with_timeout(
        "What git remote has other pods? One word, all uppercase.",
        Duration::from_secs(30),
    );
    session.wait_for_with_timeout("RUMPELPOD", Duration::from_secs(45));
}
