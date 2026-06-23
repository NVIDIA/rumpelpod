// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test that the rumpelpod system prompt is injected into
//! ~/.codex/AGENTS.md inside the container and visible to the Codex
//! agent.

use super::common::{setup_codex_test_repo, CodexSession};

#[test]
fn codex_system_prompt_describes_remotes() {
    let (_home, repo, _executor, daemon) = setup_codex_test_repo();

    let mut session = CodexSession::spawn(&repo, &daemon, _home.path(), &[]);
    session.dismiss_dialogs();

    // "all uppercase" prevents a false positive: "rumpelpod" appears
    // in the Codex warning about the temp home dir path.
    session.send("What git remote has other pods? One word, all uppercase.");
    session.wait_for("RUMPELPOD");
}

#[test]
fn codex_system_prompt_describes_description_file() {
    let (_home, repo, _executor, daemon) = setup_codex_test_repo();

    let mut session = CodexSession::spawn(&repo, &daemon, _home.path(), &[]);
    session.dismiss_dialogs();

    session.send("In which file should you put the merge commit message? One word only.");
    session.wait_for("DESCRIPTION");
}
