// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Smoke test: verify that the pi CLI running inside the container can
//! reach the Anthropic API through the pod server's LLM cache proxy and
//! answer a basic prompt.

use super::common::{setup_pi_test_repo, PiSession};

#[test]
fn pi_smoke() {
    // pi runs on Node, so building the prepared image and starting the
    // TUI is heavier than claude/codex; give it headroom over the 120s
    // default for cold builds and high-parallelism runs.
    println!("xtest:timeout=240");
    let (home, repo, _executor, daemon) = setup_pi_test_repo();

    let mut session = PiSession::spawn(
        &repo,
        &daemon,
        home.path(),
        &["--model", "anthropic/claude-haiku-4-5"],
    );

    session.wait_until_ready();

    session.send("What is the capital of France? Reply with just the city name, nothing else.");
    session.wait_for("Paris");
}
