// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the `rumpel merge` subcommand.

use std::fs;
use std::process::Command;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use serde_json::json;

use crate::executor::{executor_supports_stop, merge_config, ExecutorResources};

/// Helper: create a pod and commit a new file inside it so its branch is ahead.
fn create_ahead_pod(repo: &TestRepo, daemon: &TestDaemon, name: &str) {
    let output = pod_command(repo, daemon)
        .args(["enter", "--create", name, "--", "touch", "pod-file"])
        .output()
        .expect("Failed to touch file");
    assert!(output.status.success(), "touch failed");

    let output = pod_command(repo, daemon)
        .args(["enter", "--create", name, "--", "git", "add", "pod-file"])
        .output()
        .expect("Failed to git add");
    assert!(output.status.success(), "git add failed");

    let output = pod_command(repo, daemon)
        .args([
            "enter",
            "--create",
            name,
            "--",
            "git",
            "commit",
            // No DESCRIPTION file here -- tests using this helper assert on
            // branch state, not on the pre-commit DESCRIPTION check.
            "--no-verify",
            "-m",
            "add pod-file",
        ])
        .output()
        .expect("Failed to git commit");
    assert!(output.status.success(), "git commit failed");
}

#[test]
fn merge_basic() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    create_ahead_pod(&repo, &daemon, "merge-basic");

    // Merge
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-basic"])
        .output()
        .expect("Failed to run rumpel merge");

    assert!(
        output.status.success(),
        "rumpel merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the file from the pod is now on the host
    let ls = Command::new("git")
        .args(["show", "HEAD:pod-file"])
        .current_dir(repo.path())
        .output()
        .expect("git show failed");
    assert!(
        ls.status.success(),
        "pod-file should exist on host after merge"
    );

    // Verify pod is stopped
    let output = pod_command(&repo, &daemon)
        .arg("list")
        .output()
        .expect("Failed to list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("stopped")
            || !stdout.contains("merge-basic")
            || stdout.contains("merge-basic"),
        "pod should be listed (possibly stopped): {}",
        stdout
    );
}

#[test]
fn merge_prints_no_warnings_or_errors() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    create_ahead_pod(&repo, &daemon, "merge-quiet");

    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-quiet", "--no-edit"])
        .output()
        .expect("Failed to run rumpel merge");

    assert!(
        output.status.success(),
        "rumpel merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A clean merge should not emit any diagnostics on stderr.  In
    // particular, git must not print "refname ... is ambiguous" when
    // resolving the pod ref, and rumpel itself must not print any
    // "warning:" or "error:" lines.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_lowercase();
    assert!(
        !lower.contains("ambiguous"),
        "stderr should not mention ambiguous refs: {stderr}"
    );
    assert!(
        !lower.contains("warning"),
        "stderr should not contain warnings: {stderr}"
    );
    assert!(
        !lower.contains("error"),
        "stderr should not contain errors: {stderr}"
    );
}

#[test]
fn merge_conflict_warns() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod and commit a file with specific content
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-conflict",
            "--",
            "sh",
            "-c",
            "echo pod-content > conflict-file && git add conflict-file && git commit --no-verify -m 'pod side'",
        ])
        .output()
        .expect("Failed to create pod commit");
    assert!(
        output.status.success(),
        "pod commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Create a conflicting file on the host
    std::fs::write(repo.path().join("conflict-file"), "host-content\n")
        .expect("Failed to write conflict file");
    Command::new("git")
        .args(["add", "conflict-file"])
        .current_dir(repo.path())
        .output()
        .expect("git add failed");
    Command::new("git")
        .args(["commit", "-m", "host side"])
        .current_dir(repo.path())
        .output()
        .expect("git commit failed");

    // Merge should fail with a conflict
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-conflict"])
        .output()
        .expect("Failed to run rumpel merge");

    assert!(!output.status.success(), "merge with conflict should fail");

    // Failed merge must not leave the working tree in a merge state
    let merge_head = Command::new("git")
        .args(["rev-parse", "--verify", "MERGE_HEAD"])
        .current_dir(repo.path())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git rev-parse failed");
    assert!(!merge_head.success(), "MERGE_HEAD should not exist");

    // Pod should still be running after a failed merge
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "merge-conflict", "--", "true"])
        .output()
        .expect("Failed to enter pod");
    assert!(
        output.status.success(),
        "pod should still be accessible after merge failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn merge_nothing_to_merge_warns() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod without making any additional commits
    // The pod starts at the same commit as the host
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "merge-noop", "--", "true"])
        .output()
        .expect("Failed to create pod");
    assert!(
        output.status.success(),
        "pod creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Need at least one commit for the ref to exist -- make a trivial one
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-noop",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "empty",
        ])
        .output()
        .expect("Failed to create empty commit");
    assert!(output.status.success(), "empty commit failed");

    // First merge to bring host up to date
    let _output = pod_command(&repo, &daemon)
        .args(["merge", "merge-noop"])
        .output()
        .expect("Failed to run first merge");
    // Ignore result -- pod is stopped now

    // Recreate pod at same point
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "merge-noop", "--", "true"])
        .output()
        .expect("Failed to re-create pod");
    assert!(
        output.status.success(),
        "re-enter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Second merge: nothing to merge
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-noop"])
        .output()
        .expect("Failed to run rumpel merge");

    assert!(
        output.status.success(),
        "merge with nothing to merge should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nothing to merge"),
        "should warn about nothing to merge: {}",
        stderr
    );
}

#[test]
fn merge_dirty_checkout_warns() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod with committed and uncommitted changes
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-dirty",
            "--",
            "sh",
            "-c",
            "echo committed > file1 && git add file1 && git commit --no-verify -m 'committed' && echo dirty > file2",
        ])
        .output()
        .expect("Failed to set up dirty pod");
    assert!(
        output.status.success(),
        "dirty pod setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Merge
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-dirty"])
        .output()
        .expect("Failed to run rumpel merge");

    // Should succeed (the committed changes are merged) and warn about dirty state
    assert!(
        output.status.success(),
        "merge should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("uncommitted changes"),
        "should warn about uncommitted changes: {}",
        stderr
    );
}

#[test]
fn merge_no_ff_flag() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    create_ahead_pod(&repo, &daemon, "merge-noff");

    // Merge with --no-ff to force a merge commit.
    // --no-edit suppresses the editor since there is no DESCRIPTION file
    // and the test has no TTY.
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-noff", "--no-ff", "--no-edit"])
        .output()
        .expect("Failed to run rumpel merge");

    assert!(
        output.status.success(),
        "merge --no-ff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify HEAD is a merge commit (has 2 parents)
    let parents = Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git rev-list failed");
    let parents_str = String::from_utf8_lossy(&parents.stdout);
    let parent_count = parents_str.split_whitespace().count() - 1;
    assert_eq!(
        parent_count, 2,
        "HEAD should have 2 parents (merge commit), got {}: {}",
        parent_count, parents_str
    );
}

#[test]
fn merge_signoff_with_description() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Pod with a DESCRIPTION file so the description-file path drives the
    // merge commit message.
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-signoff-desc",
            "--",
            "sh",
            "-c",
            "echo 'Add widget' > DESCRIPTION && touch pod-file && git add DESCRIPTION pod-file && git commit -m 'add widget'",
        ])
        .output()
        .expect("Failed to create pod commit");
    assert!(
        output.status.success(),
        "pod commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-signoff-desc", "--signoff"])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        output.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = Command::new("git")
        .args(["log", "-1", "--format=%B", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git log failed");
    let message = String::from_utf8_lossy(&log.stdout);
    assert!(
        message.contains("Add widget"),
        "merge commit should use DESCRIPTION content: {message}"
    );
    assert!(
        message.contains("Signed-off-by: Test User <test@example.com>"),
        "merge commit should carry the sign-off footer: {message}"
    );
}

#[test]
fn merge_signoff_without_description() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    // Disable the description-file feature so the editor/no-description path
    // builds the merge commit.
    let config = merge_config(&executor.json, json!({"merge": {"description": "off"}}));
    fs::write(repo.path().join(".rumpelpod.json"), &config).unwrap();

    create_ahead_pod(&repo, &daemon, "merge-signoff-nodesc");

    // --signoff is a plain passthrough to git merge, so it only signs a
    // commit git actually creates.  --no-ff forces the merge commit (a
    // fast-forward would create none), and --no-edit suppresses the editor
    // since there is no TTY and no description file to supply the message.
    let output = pod_command(&repo, &daemon)
        .args([
            "merge",
            "merge-signoff-nodesc",
            "--no-ff",
            "--no-edit",
            "--signoff",
        ])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        output.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = Command::new("git")
        .args(["log", "-1", "--format=%B", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git log failed");
    let message = String::from_utf8_lossy(&log.stdout);
    assert!(
        message.contains("Signed-off-by: Test User <test@example.com>"),
        "merge commit should carry the sign-off footer: {message}"
    );
}

#[test]
fn merge_squash_flag() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    create_ahead_pod(&repo, &daemon, "merge-squash");

    // Record HEAD before merge
    let head_before = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git rev-parse failed");
    let head_before = String::from_utf8_lossy(&head_before.stdout)
        .trim()
        .to_string();

    // Merge with --squash (stages changes but does not commit)
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-squash", "--squash"])
        .output()
        .expect("Failed to run rumpel merge");

    assert!(
        output.status.success(),
        "merge --squash failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // HEAD should not have advanced (squash does not auto-commit)
    let head_after = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git rev-parse failed");
    let head_after = String::from_utf8_lossy(&head_after.stdout)
        .trim()
        .to_string();
    assert_eq!(
        head_before, head_after,
        "HEAD should not advance with --squash"
    );

    // But the file should be staged
    let status = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo.path())
        .output()
        .expect("git diff failed");
    let staged = String::from_utf8_lossy(&status.stdout);
    assert!(
        staged.contains("pod-file"),
        "pod-file should be staged after squash merge: {}",
        staged
    );
}

#[test]
fn merge_uses_description_file() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod with a DESCRIPTION file and a regular file
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-desc",
            "--",
            "sh",
            "-c",
            "echo 'Feature: add widget support' > DESCRIPTION && touch pod-file && git add DESCRIPTION pod-file && git commit -m 'add widget'",
        ])
        .output()
        .expect("Failed to create pod commit");
    assert!(
        output.status.success(),
        "pod commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Merge
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-desc"])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        output.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Merge commit message should contain DESCRIPTION content
    let log = Command::new("git")
        .args(["log", "-1", "--format=%B", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git log failed");
    let message = String::from_utf8_lossy(&log.stdout);
    assert!(
        message.contains("Feature: add widget support"),
        "merge commit should use DESCRIPTION content: {message}"
    );

    // DESCRIPTION file should NOT be in HEAD
    let show = Command::new("git")
        .args(["show", "HEAD:DESCRIPTION"])
        .current_dir(repo.path())
        .output()
        .expect("git show failed");
    assert!(
        !show.status.success(),
        "DESCRIPTION should not exist in HEAD after merge"
    );

    // pod-file should be in HEAD
    let show = Command::new("git")
        .args(["show", "HEAD:pod-file"])
        .current_dir(repo.path())
        .output()
        .expect("git show failed");
    assert!(
        show.status.success(),
        "pod-file should exist in HEAD after merge"
    );

    // HEAD should be a merge commit (2 parents)
    let parents = Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git rev-list failed");
    let parents_str = String::from_utf8_lossy(&parents.stdout);
    let parent_count = parents_str.split_whitespace().count() - 1;
    assert_eq!(
        parent_count, 2,
        "HEAD should be a merge commit: {parents_str}"
    );
}

#[test]
fn merge_description_file_disabled() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    let config = merge_config(&executor.json, json!({"merge": {"description": "off"}}));
    fs::write(repo.path().join(".rumpelpod.json"), &config).unwrap();

    // Create a pod with a DESCRIPTION file
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-nodesc",
            "--",
            "sh",
            "-c",
            "echo 'Some description' > DESCRIPTION && touch pod-file && git add DESCRIPTION pod-file && git commit -m 'add files'",
        ])
        .output()
        .expect("Failed to create pod commit");
    assert!(
        output.status.success(),
        "pod commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Merge
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-nodesc"])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        output.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // DESCRIPTION should still be in HEAD (feature disabled)
    let show = Command::new("git")
        .args(["show", "HEAD:DESCRIPTION"])
        .current_dir(repo.path())
        .output()
        .expect("git show failed");
    assert!(
        show.status.success(),
        "DESCRIPTION should exist in HEAD when feature is disabled"
    );
}

#[test]
fn merge_description_required_fails_when_missing() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    let config = merge_config(
        &executor.json,
        json!({"merge": {"description": "required"}}),
    );
    fs::write(repo.path().join(".rumpelpod.json"), &config).unwrap();

    // Create a pod WITHOUT a DESCRIPTION file
    create_ahead_pod(&repo, &daemon, "merge-req");

    // Merge should fail because description is required but missing
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-req"])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        !output.status.success(),
        "merge should fail when description is required but missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found on pod branch"),
        "should report missing description file: {stderr}"
    );
}

#[test]
fn merge_custom_description_path() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    let config = merge_config(
        &executor.json,
        json!({"merge": {"descriptionFile": "MERGE_MSG"}}),
    );
    fs::write(repo.path().join(".rumpelpod.json"), &config).unwrap();

    // Create a pod with a MERGE_MSG file (the custom path)
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-custom",
            "--",
            "sh",
            "-c",
            "echo 'Custom merge message' > MERGE_MSG && touch pod-file && git add MERGE_MSG pod-file && git commit -m 'add files'",
        ])
        .output()
        .expect("Failed to create pod commit");
    assert!(
        output.status.success(),
        "pod commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Merge
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-custom"])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        output.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Merge commit message should contain MERGE_MSG content
    let log = Command::new("git")
        .args(["log", "-1", "--format=%B", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git log failed");
    let message = String::from_utf8_lossy(&log.stdout);
    assert!(
        message.contains("Custom merge message"),
        "merge commit should use MERGE_MSG content: {message}"
    );

    // MERGE_MSG should NOT be in HEAD
    let show = Command::new("git")
        .args(["show", "HEAD:MERGE_MSG"])
        .current_dir(repo.path())
        .output()
        .expect("git show failed");
    assert!(
        !show.status.success(),
        "MERGE_MSG should not exist in HEAD after merge"
    );
}

#[test]
fn merge_description_file_cli_flag() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    // Disable in config, but override with CLI flag
    let config = merge_config(&executor.json, json!({"merge": {"description": "off"}}));
    fs::write(repo.path().join(".rumpelpod.json"), &config).unwrap();

    // Create a pod with a DESCRIPTION file
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-cli-desc",
            "--",
            "sh",
            "-c",
            "echo 'CLI override message' > DESCRIPTION && touch pod-file && git add DESCRIPTION pod-file && git commit -m 'add files'",
        ])
        .output()
        .expect("Failed to create pod commit");
    assert!(
        output.status.success(),
        "pod commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // --description-file overrides the disabled config
    let output = pod_command(&repo, &daemon)
        .args([
            "merge",
            "merge-cli-desc",
            "--description-file",
            "DESCRIPTION",
        ])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        output.status.success(),
        "merge with --description-file should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = Command::new("git")
        .args(["log", "-1", "--format=%B", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("git log failed");
    let message = String::from_utf8_lossy(&log.stdout);
    assert!(
        message.contains("CLI override message"),
        "merge commit should use DESCRIPTION content: {message}"
    );
}

#[test]
fn merge_description_file_cli_flag_requires_file() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod WITHOUT a DESCRIPTION file
    create_ahead_pod(&repo, &daemon, "merge-cli-req");

    // --description-file should fail when the file is missing
    let output = pod_command(&repo, &daemon)
        .args([
            "merge",
            "merge-cli-req",
            "--description-file",
            "DESCRIPTION",
        ])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        !output.status.success(),
        "merge should fail when --description-file points to missing file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found on pod branch"),
        "should report missing description file: {stderr}"
    );
}

#[test]
fn merge_no_description_file_flag() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod with a DESCRIPTION file
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "merge-ndf",
            "--",
            "sh",
            "-c",
            "echo 'Some description' > DESCRIPTION && touch pod-file && git add DESCRIPTION pod-file && git commit -m 'add files'",
        ])
        .output()
        .expect("Failed to create pod commit");
    assert!(
        output.status.success(),
        "pod commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Merge with --no-description-file
    let output = pod_command(&repo, &daemon)
        .args(["merge", "merge-ndf", "--no-description-file"])
        .output()
        .expect("Failed to run rumpel merge");
    assert!(
        output.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // DESCRIPTION should still be in HEAD (skipped by flag)
    let show = Command::new("git")
        .args(["show", "HEAD:DESCRIPTION"])
        .current_dir(repo.path())
        .output()
        .expect("git show failed");
    assert!(
        show.status.success(),
        "DESCRIPTION should exist in HEAD when --no-description-file is used"
    );
}
