// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::common::{setup_codex_test_repo, CodexSession};

use crate::common::{write_test_devcontainer, TestRepo};

const CTRL_A: u8 = 0x01;

#[test]
fn codex_pty_session_is_scoped_by_repo_and_pod() {
    let (home, repo_one, executor, daemon) = setup_codex_test_repo();
    let repo_two = TestRepo::new();
    write_test_devcontainer(&repo_two, "", "");
    std::fs::write(repo_two.path().join(".rumpelpod.json"), &executor.json)
        .expect("write second .rumpelpod.json");

    let marker = "repo-one-unsubmitted-codex-marker";

    let mut first = CodexSession::spawn(&repo_one, &daemon, home.path(), &[]);
    first.dismiss_dialogs();
    first.write_raw(marker.as_bytes());
    first.wait_for(marker);
    first.write_raw(&[CTRL_A, b'd']);
    first.wait_for_exit();

    let mut second = CodexSession::spawn(&repo_two, &daemon, home.path(), &[]);
    let contents = second.wait_for("\u{203a}");

    assert!(
        !contents.contains(marker),
        "second repo reattached to first repo's codex TUI:\n{contents}"
    );
}
