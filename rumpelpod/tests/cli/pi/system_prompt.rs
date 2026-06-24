// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test that the rumpelpod system prompt is injected into the container
//! (written to ~/.pi/agent/SYSTEM.md) and visible to the pi CLI.

use super::common::{setup_pi_test_repo, PiSession};

#[test]
fn pi_system_prompt_describes_remotes() {
    // See pi_smoke: pi's Node toolchain makes cold image build + TUI
    // startup slower than the 120s default under high parallelism.
    println!("xtest:timeout=240");
    let (home, repo, _executor, daemon) = setup_pi_test_repo();

    let mut session = PiSession::spawn(
        &repo,
        &daemon,
        home.path(),
        &["--model", "anthropic/claude-haiku-4-5"],
    );

    session.wait_until_ready();
    session.send("What git remote has other pods? One word only.");

    session.wait_for("rumpelpod");
}
