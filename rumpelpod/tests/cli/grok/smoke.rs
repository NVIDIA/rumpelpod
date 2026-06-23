// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Smoke test: verify that `rumpel grok` brings the grok CLI up inside
//! the container, that the forwarded API key is recognized, and that the
//! TUI renders its interactive prompt through rumpelpod's PTY plumbing.
//!
//! Unlike the claude and codex smoke tests, this does not assert a model
//! response.  grok's chat/inference client always targets the public xAI
//! API and offers no base-URL override we can point at the LLM cache
//! proxy, so a cached, offline-deterministic answer is not feasible.  The
//! test instead exercises the full rumpelpod integration surface: the
//! prepared image carries the grok binary, the `/grok` PTY route attaches
//! a session, and the credential reaches grok inside the pod.

use super::common::{setup_grok_test_repo, GrokSession, GROK_TEST_MODEL};

#[test]
fn grok_smoke() {
    let (home, repo, _executor, daemon) = setup_grok_test_repo();

    let mut session = GrokSession::spawn(&repo, &daemon, home.path(), GROK_TEST_MODEL);

    // The status line shows "Logged in with API key" alongside the
    // "Grok Build" banner once the TUI has authenticated with the
    // forwarded key and finished rendering its prompt.
    session.wait_for("Logged in with API key");
}
