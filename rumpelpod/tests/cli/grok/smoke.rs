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

use super::common::{
    setup_grok_online_test_repo, setup_grok_test_repo, GrokSession, GROK_TEST_MODEL,
};

#[test]
fn grok_smoke() {
    let (home, repo, _executor, daemon) = setup_grok_test_repo();

    let mut session = GrokSession::spawn(&repo, &daemon, home.path(), GROK_TEST_MODEL);

    // The status line shows "Logged in with API key" alongside the
    // "Grok Build" banner once the TUI has authenticated with the
    // forwarded key and finished rendering its prompt.
    session.wait_for("Logged in with API key");
}

/// Opt-in online variant: verify grok produces a real model response.
///
/// grok's inference client always targets the public xAI API, so this
/// cannot run against the offline cache proxy and makes a real (billable)
/// xAI API call.  It runs only when `RUMPELPOD_TEST_GROK_ONLINE` is set
/// and a real `XAI_API_KEY` is present; otherwise it is skipped.  Run it
/// with `RUMPELPOD_TEST_GROK_ONLINE=1 XAI_API_KEY=... cargo pipeline
/// grok_paris_response`.
#[test]
fn grok_paris_response() {
    // Building the prepared image plus the live API round-trip can exceed
    // the default per-test timeout.
    println!("xtest:timeout=300");

    if std::env::var("RUMPELPOD_TEST_GROK_ONLINE").is_err() {
        crate::executor::skip_test();
        return;
    }
    let api_key = match std::env::var("XAI_API_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            crate::executor::skip_test();
            return;
        }
    };

    let (home, repo, _executor, daemon) = setup_grok_online_test_repo(&api_key);

    let mut session = GrokSession::spawn(&repo, &daemon, home.path(), GROK_TEST_MODEL);

    session.wait_for("Logged in with API key");
    // Keep the prompt to a single input-box line: a wrapped prompt splits
    // the echoed text across rows in the vt100 grid, so `send`'s
    // echo-confirmation needle would never match contiguously.
    session.send("What is the capital of France? Answer in one word.");
    session.wait_for("Paris");
}
