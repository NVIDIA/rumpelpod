// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Smoke test: verify a simple prompt reaches the grok CLI inside the
//! container and produces a visible response via the caching proxy.
//!
//! grok's chat client does not honor a top-level base-URL override, but
//! it does honor a per-model `base_url` in `~/.grok/config.toml`, which
//! the test setup points at the pod server's LLM cache proxy.  That lets
//! this run deterministically offline like the claude and codex smoke
//! tests.

use super::common::{setup_grok_test_repo, GrokSession};

#[test]
fn grok_smoke() {
    // On a cold cache the prepared image build downloads the ~142 MiB
    // grok binary; give it headroom over the default 120s timeout on
    // slow networks (the grok binary is much larger than codex's).
    println!("xtest:timeout=180");

    let (home, repo, _executor, daemon) = setup_grok_test_repo();

    let mut session = GrokSession::spawn(&repo, &daemon, home.path());

    // Wait for the TUI to finish loading before typing.  The input box
    // border shows "always-approve" once grok has rendered its prompt and
    // is ready to accept input; typing earlier races startup and the
    // keystrokes are lost.
    session.wait_for("always-approve");
    // Keep the prompt to a single input-box line: a wrapped prompt splits
    // the echoed text across vt100 rows, so `send`'s echo-confirmation
    // needle would never match contiguously.
    session.send("What is the capital of France? Answer in one word.");

    session.wait_for("Paris");
}
