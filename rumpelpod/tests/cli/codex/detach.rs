// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test detach (Ctrl-a d) and reattach for the host-side codex screen
//! session: have a short conversation, detach, reattach, and verify the
//! screen replay restores the conversation immediately (no welcome
//! dialog, no need to re-dismiss model selection).

use super::common::{setup_codex_test_repo, CodexSession};

/// Ctrl-a (0x01) followed by 'd' triggers detach in pty_attach.
const CTRL_A: u8 = 0x01;

#[test]
fn codex_detach_reattach() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();

    // -- First session: dummy conversation, then detach -----------------

    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.dismiss_dialogs();
    session.send("What is the capital of France? Reply with just the city name, nothing else.");
    session.wait_for("Paris");

    session.write_raw(&[CTRL_A, b'd']);
    session.wait_for_exit();

    // -- Second session: reattach and verify screen replay --------------
    //
    // The daemon-managed screen session keeps the codex TUI process
    // alive between invocations.  Reattaching replays the vt100 screen
    // buffer so the previous question and answer are visible
    // immediately, with no dismiss_dialogs() needed.

    let mut session2 = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session2.wait_for("Paris");
}
