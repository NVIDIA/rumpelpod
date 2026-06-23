// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Smoke test: verify that the codex TUI on the local machine can communicate
//! with the codex app-server inside the container through the pod
//! server's WebSocket proxy, with API requests cached via the LLM
//! cache proxy.

use super::common::{setup_codex_test_repo, CodexSession};

#[test]
fn codex_smoke() {
    let (_home, repo, _executor, daemon) = setup_codex_test_repo();

    // rumpel codex passes --dangerously-bypass-approvals-and-sandbox by
    // default; pass no model so Codex uses its own default.
    let mut session = CodexSession::spawn(&repo, &daemon, _home.path(), &[]);

    // Dismiss any startup dialogs (model selection, announcements)
    // by pressing Enter whenever the TUI is waiting.
    session.dismiss_dialogs();

    session.send("What is the capital of France? Reply with just the city name, nothing else.");
    session.wait_for("Paris");
}
