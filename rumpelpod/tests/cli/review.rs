// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the daemon `/review` API.
//!
//! The unpublished VS Code extension still POSTs this endpoint. These
//! tests lock the JSON shape it depends on until that client diffs
//! `rumpelpod/<pod>` itself.

use std::fs;
use std::process::Command;

use rumpelpod::daemon::protocol::{Daemon, DaemonClient, PodName};
use rumpelpod::CommandExt;

use crate::common::{
    create_commit, pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo,
};
use crate::executor::ExecutorResources;

#[test]
fn review_api_describes_added_deleted_and_renamed_files() {
    let repo = TestRepo::new();
    let deleted_file = "deleted.txt";
    let renamed_file = "renamed-before.txt";
    let renamed_target = "renamed-after.txt";
    fs::write(repo.path().join(deleted_file), "delete this\n").expect("write deleted fixture");
    fs::write(repo.path().join(renamed_file), "rename this\n").expect("write renamed fixture");
    Command::new("git")
        .args(["add", deleted_file, renamed_file])
        .current_dir(repo.path())
        .success()
        .expect("stage review fixtures");
    create_commit(repo.path(), "Add review JSON fixtures");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-utf8-file";
    let file_name = "review-\u{00e4}.txt";

    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    let command = format!(
        "rm '{deleted_file}' && git mv '{renamed_file}' '{renamed_target}' && \
         printf '%s\\n' 'utf8 review content' > '{file_name}' && \
         git add -A && git commit --no-verify -m 'Change review files'"
    );
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "sh", "-c", &command])
        .success()
        .expect("Failed to commit review files in pod");

    let client = DaemonClient::new_unix(&daemon.socket_path);
    let plan = client
        .review_plan(
            repo.path().to_path_buf(),
            PodName::new(pod_name).expect("valid pod name"),
            Vec::new(),
        )
        .expect("request review plan from daemon");
    let files = &plan.files;
    let file = |path: &str| {
        files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("review plan omitted '{path}': {files:?}"))
    };
    for path in [file_name, renamed_target] {
        assert!(!file(path).base_exists);
        assert!(file(path).target_exists);
    }
    for path in [deleted_file, renamed_file] {
        assert!(file(path).base_exists);
        assert!(!file(path).target_exists);
    }
}

#[test]
fn review_api_works_in_detached_head() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("file.txt"), "initial content\n").expect("Failed to write file");
    Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");
    create_commit(repo.path(), "Add file.txt");

    let head_commit: String = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .success()
        .expect("git rev-parse failed")
        .try_into()
        .unwrap();
    let head_commit = head_commit.trim();

    Command::new("git")
        .args(["checkout", "--detach", head_commit])
        .current_dir(repo.path())
        .success()
        .expect("git checkout --detach failed");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-detached-head";

    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            "echo 'modified content' > file.txt && git add file.txt && \
             git commit --no-verify -m 'Modify file in pod'",
        ])
        .success()
        .expect("Failed to commit in pod");

    let client = DaemonClient::new_unix(&daemon.socket_path);
    let plan = client
        .review_plan(
            repo.path().to_path_buf(),
            PodName::new(pod_name).expect("valid pod name"),
            Vec::new(),
        )
        .expect("review plan should work in detached HEAD");
    let file = plan
        .files
        .iter()
        .find(|file| file.path == "file.txt")
        .expect("review plan omitted file.txt");
    assert!(file.base_exists);
    assert!(file.target_exists);
}

#[test]
fn review_api_returns_no_files_when_unchanged() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-no-changes";

    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Empty commit",
        ])
        .success()
        .expect("Failed to commit in pod");

    let client = DaemonClient::new_unix(&daemon.socket_path);
    let plan = client
        .review_plan(
            repo.path().to_path_buf(),
            PodName::new(pod_name).expect("valid pod name"),
            Vec::new(),
        )
        .expect("review plan should succeed with no file changes");
    assert!(
        plan.files.is_empty(),
        "unchanged pod should have an empty file list, got: {:?}",
        plan.files
    );
}

#[test]
fn review_api_rejects_unknown_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let client = DaemonClient::new_unix(&daemon.socket_path);
    let error = client
        .review_plan(
            repo.path().to_path_buf(),
            PodName::new("does-not-exist").expect("valid pod name"),
            Vec::new(),
        )
        .expect_err("review plan should fail for a non-existent pod");
    let message = format!("{error:#}");
    assert!(
        message.contains("does not exist"),
        "Error should say pod does not exist, got: {message}"
    );
}
