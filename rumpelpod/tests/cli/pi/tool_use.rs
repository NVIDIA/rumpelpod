// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests verifying that the pi CLI can use tools (read and write files)
//! inside the container, exercised end-to-end through the caching proxy.

use std::process::Command;
use std::time::{Duration, Instant};

use rumpelpod::CommandExt;

use super::common::{setup_pi_test_repo, PiSession};
use crate::common::{create_commit, pod_command};

#[test]
fn pi_read_file() {
    // See pi_smoke: pi's Node toolchain makes cold image build + TUI
    // startup slower than the 120s default under high parallelism.
    println!("xtest:timeout=240");
    let (home, repo, _executor, daemon) = setup_pi_test_repo();

    // Commit a file so the gateway syncs it into the container.
    std::fs::write(repo.path().join("hello.txt"), "rumpelpod-test-content-42\n")
        .expect("write hello.txt to test repo");
    Command::new("git")
        .args(["add", "hello.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add hello.txt");
    create_commit(repo.path(), "Add hello.txt");

    let mut session = PiSession::spawn(
        &repo,
        &daemon,
        home.path(),
        &["--model", "anthropic/claude-haiku-4-5"],
    );

    session.wait_until_ready();
    session.send("Read hello.txt. Reply with only the file contents.");

    // The file content only appears on screen after pi reads it, not in
    // the prompt itself.
    session.wait_for("rumpelpod-test-content-42");
}

#[test]
fn pi_write_file() {
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
    session.send("Write 'rumpelpod-write-ok' to output.txt");

    // Poll via `rumpel enter` until the file appears inside the
    // container.  This avoids relying on screen output matching (the
    // prompt text already contains the filename and content).
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let result = pod_command(&repo, &daemon)
            .args(["enter", "--create", "test", "--", "cat", "output.txt"])
            .output()
            .expect("rumpel enter failed to execute");

        if result.status.success() {
            let stdout = String::from_utf8_lossy(&result.stdout);
            if stdout.contains("rumpelpod-write-ok") {
                break;
            }
        }

        assert!(
            Instant::now() < deadline,
            "output.txt not created with expected content within timeout",
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}
