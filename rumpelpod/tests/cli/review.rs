// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the `rumpel review` command.
//!
//! These tests verify that `rumpel review` correctly:
//! - Computes the merge base between the pod branch and its upstream
//! - Invokes git difftool with the correct commits
//! - Handles error cases (no upstream, pod not found)

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use rumpelpod::CommandExt;

use crate::common::{
    create_commit, pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo,
};
use crate::executor::ExecutorResources;

/// Create a mock difftool script that logs the files it sees.
/// Returns the path to the log file.
fn setup_mock_difftool(repo_path: &Path) -> std::path::PathBuf {
    let log_file = repo_path.join("difftool.log");
    let script_path = repo_path.join("mock-difftool.sh");

    // Create the mock difftool script
    // The script logs the arguments and file contents to the log file
    let script = format!(
        r#"#!/bin/sh
# Mock difftool for testing - logs the files it receives
LOCAL="$1"
REMOTE="$2"

# Log the file paths
echo "=== DIFF ===" >> "{log}"
echo "LOCAL: $LOCAL" >> "{log}"
echo "REMOTE: $REMOTE" >> "{log}"

# Log the content of both files
echo "--- LOCAL CONTENT ---" >> "{log}"
cat "$LOCAL" >> "{log}" 2>/dev/null || echo "(empty or missing)" >> "{log}"
echo "" >> "{log}"
echo "--- REMOTE CONTENT ---" >> "{log}"
cat "$REMOTE" >> "{log}" 2>/dev/null || echo "(empty or missing)" >> "{log}"
echo "" >> "{log}"
"#,
        log = log_file.display()
    );

    fs::write(&script_path, script).expect("Failed to write mock difftool script");

    // Make the script executable
    let mut perms = fs::metadata(&script_path)
        .expect("Failed to get script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).expect("Failed to set script permissions");

    // Configure git to use our mock difftool (local config only)
    Command::new("git")
        .args(["config", "diff.tool", "mock"])
        .current_dir(repo_path)
        .success()
        .expect("Failed to set diff.tool");

    let cmd = format!("{} \"$LOCAL\" \"$REMOTE\"", script_path.display());
    Command::new("git")
        .args(["config", "difftool.mock.cmd", &cmd])
        .current_dir(repo_path)
        .success()
        .expect("Failed to set difftool.mock.cmd");

    // Disable prompt for difftool (in addition to -y flag)
    Command::new("git")
        .args(["config", "difftool.prompt", "false"])
        .current_dir(repo_path)
        .success()
        .expect("Failed to set difftool.prompt");

    log_file
}

/// Get the content of the difftool log file.
fn read_difftool_log(log_path: &Path) -> String {
    fs::read_to_string(log_path).unwrap_or_default()
}

#[test]
fn review_shows_pod_changes() {
    let repo = TestRepo::new();

    // Create a file in the initial state
    fs::write(repo.path().join("file.txt"), "initial content\n").expect("Failed to write file");
    Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");
    create_commit(repo.path(), "Add file.txt");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Make a change in the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            "echo 'modified content' > file.txt",
        ])
        .success()
        .expect("Failed to modify file in pod");

    // Commit the change in the pod
    pod_command(&repo, &daemon)
        .args([
            "enter", "--create", pod_name, "--", "git", "add", "file.txt",
        ])
        .success()
        .expect("Failed to stage file in pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "Modify file in pod",
        ])
        .success()
        .expect("Failed to commit in pod");

    // Set up mock difftool
    let log_file = setup_mock_difftool(repo.path());

    // Run review command
    pod_command(&repo, &daemon)
        .args(["review", pod_name, "--yes"])
        .success()
        .expect("rumpel review failed");

    // Verify the difftool was called with the expected content
    let log = read_difftool_log(&log_file);

    assert!(
        log.contains("=== DIFF ==="),
        "Log should contain diff marker, got: {}",
        log
    );
    assert!(
        log.contains("initial content"),
        "Log should contain original content (merge base), got: {}",
        log
    );
    assert!(
        log.contains("modified content"),
        "Log should contain modified content (pod), got: {}",
        log
    );
}

#[test]
fn review_shows_new_files() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-new-file";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a new file in the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            "echo 'new file content' > newfile.txt",
        ])
        .success()
        .expect("Failed to create file in pod");

    // Commit the new file
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "add",
            "newfile.txt",
        ])
        .success()
        .expect("Failed to stage new file in pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "Add new file in pod",
        ])
        .success()
        .expect("Failed to commit in pod");

    // Set up mock difftool
    let log_file = setup_mock_difftool(repo.path());

    // Run review command
    pod_command(&repo, &daemon)
        .args(["review", pod_name, "--yes"])
        .success()
        .expect("rumpel review failed");

    // Verify the difftool was called with the new file
    let log = read_difftool_log(&log_file);

    assert!(
        log.contains("new file content"),
        "Log should contain new file content, got: {}",
        log
    );
}

#[test]
fn review_multiple_files() {
    let repo = TestRepo::new();

    // Create multiple files
    fs::write(repo.path().join("file1.txt"), "file1 original\n").expect("Failed to write file1");
    fs::write(repo.path().join("file2.txt"), "file2 original\n").expect("Failed to write file2");
    Command::new("git")
        .args(["add", "file1.txt", "file2.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");
    create_commit(repo.path(), "Add files");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-multi";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Modify both files in the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            "echo 'file1 modified' > file1.txt && echo 'file2 modified' > file2.txt",
        ])
        .success()
        .expect("Failed to modify files in pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "add",
            "file1.txt",
            "file2.txt",
        ])
        .success()
        .expect("Failed to stage files in pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "Modify both files",
        ])
        .success()
        .expect("Failed to commit in pod");

    // Set up mock difftool
    let log_file = setup_mock_difftool(repo.path());

    // Run review command
    pod_command(&repo, &daemon)
        .args(["review", pod_name, "--yes"])
        .success()
        .expect("rumpel review failed");

    // Verify both files were shown
    let log = read_difftool_log(&log_file);

    // Count the number of diffs (one per file)
    let diff_count = log.matches("=== DIFF ===").count();
    assert_eq!(
        diff_count, 2,
        "Should have 2 diffs (one per file), got: {}",
        diff_count
    );

    assert!(
        log.contains("file1 modified"),
        "Log should contain file1 modifications"
    );
    assert!(
        log.contains("file2 modified"),
        "Log should contain file2 modifications"
    );
}

#[test]
fn review_path_filter() {
    let repo = TestRepo::new();

    // Create multiple files
    fs::write(repo.path().join("file1.txt"), "file1 original\n").expect("Failed to write file1");
    fs::write(repo.path().join("file2.txt"), "file2 original\n").expect("Failed to write file2");
    fs::write(repo.path().join("file3.txt"), "file3 original\n").expect("Failed to write file3");
    Command::new("git")
        .args(["add", "file1.txt", "file2.txt", "file3.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");
    create_commit(repo.path(), "Add files");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-path-filter";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Modify all three files in the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            "echo 'file1 modified' > file1.txt && echo 'file2 modified' > file2.txt && echo 'file3 modified' > file3.txt",
        ])
        .success()
        .expect("Failed to modify files in pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "add",
            "file1.txt",
            "file2.txt",
            "file3.txt",
        ])
        .success()
        .expect("Failed to stage files in pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "Modify all three files",
        ])
        .success()
        .expect("Failed to commit in pod");

    // Set up mock difftool
    let log_file = setup_mock_difftool(repo.path());

    // Review with path filter -- only file1.txt and file3.txt
    pod_command(&repo, &daemon)
        .args(["review", pod_name, "--yes", "--", "file1.txt", "file3.txt"])
        .success()
        .expect("rumpel review with path filter failed");

    // Verify only the filtered files were shown
    let log = read_difftool_log(&log_file);

    let diff_count = log.matches("=== DIFF ===").count();
    assert_eq!(
        diff_count, 2,
        "Should have 2 diffs (file1 and file3 only), got: {}",
        diff_count
    );

    assert!(
        log.contains("file1 modified"),
        "Log should contain file1 modifications"
    );
    assert!(
        !log.contains("file2 modified"),
        "Log should NOT contain file2 modifications (filtered out)"
    );
    assert!(
        log.contains("file3 modified"),
        "Log should contain file3 modifications"
    );
}

#[test]
fn review_works_in_detached_head() {
    let repo = TestRepo::new();

    // Create a file before entering detached HEAD state
    fs::write(repo.path().join("file.txt"), "initial content\n").expect("Failed to write file");
    Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");
    create_commit(repo.path(), "Add file.txt");

    // Put the local repository in detached HEAD state before building the image
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

    // Launch pod (created while the local repo is in detached HEAD)
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Make a change in the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            "echo 'modified content' > file.txt",
        ])
        .success()
        .expect("Failed to modify file in pod");

    // Commit the change
    pod_command(&repo, &daemon)
        .args([
            "enter", "--create", pod_name, "--", "git", "add", "file.txt",
        ])
        .success()
        .expect("Failed to stage file in pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "commit",
            "--no-verify",
            "-m",
            "Modify file in pod",
        ])
        .success()
        .expect("Failed to commit in pod");

    // Set up mock difftool
    let log_file = setup_mock_difftool(repo.path());

    // Run review command - should succeed even in detached HEAD
    pod_command(&repo, &daemon)
        .args(["review", pod_name, "--yes"])
        .success()
        .expect("rumpel review should work in detached HEAD state");

    // Verify the difftool was called with the expected content
    let log = read_difftool_log(&log_file);
    assert!(
        log.contains("modified content"),
        "Log should contain modified content, got: {}",
        log
    );
}

#[test]
fn review_no_changes() {
    // Test that review works even when there are no changes (empty diff)
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-no-changes";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Make an empty commit so the pod ref exists
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

    // Set up mock difftool (it should not be called since there are no file changes)
    let log_file = setup_mock_difftool(repo.path());

    // Run review command - should succeed
    pod_command(&repo, &daemon)
        .args(["review", pod_name, "--yes"])
        .success()
        .expect("rumpel review should succeed with no changes");

    // The difftool log should be empty or not exist
    let log = read_difftool_log(&log_file);
    assert!(
        log.is_empty(),
        "No diffs should be shown when there are no file changes, got: {}",
        log
    );
}

/// Install a mock `code` binary into the test home's bin dir.
/// Returns the path to the log file the mock writes its args to.
fn setup_mock_vscode(home: &TestHome) -> std::path::PathBuf {
    let log_file = home.path().join("code.log");
    let script_path = home.bin_dir().join("code");

    let script = format!(
        r#"#!/bin/sh
# Mock VS Code CLI for testing -- logs all args to the log file.
echo "=== code invocation ===" >> "{log}"
for arg in "$@"; do
    echo "arg: $arg" >> "{log}"
done
"#,
        log = log_file.display()
    );

    fs::write(&script_path, script).expect("Failed to write mock code script");
    let mut perms = fs::metadata(&script_path)
        .expect("Failed to stat mock code script")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).expect("Failed to chmod mock code script");

    log_file
}

#[test]
fn review_defaults_to_vscode_when_no_difftool_configured() {
    // With no `diff.tool` set in git config, `rumpel review` should
    // fall back to VS Code if `code` is on PATH and warn the user.
    let repo = TestRepo::new();

    fs::write(repo.path().join("file.txt"), "initial\n").expect("Failed to write file");
    Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");
    create_commit(repo.path(), "Add file.txt");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-vscode-default";

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
            "echo modified > file.txt && git add file.txt && git commit --no-verify -m m",
        ])
        .success()
        .expect("Failed to modify and commit in pod");

    let code_log = setup_mock_vscode(&home);

    let output = pod_command(&repo, &daemon)
        .args(["review", pod_name, "--yes"])
        .output()
        .expect("Failed to run rumpel review");

    assert!(
        output.status.success(),
        "rumpel review should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("using VS Code"),
        "stderr should carry the warning, got: {stderr}"
    );
    assert!(
        stderr.contains("tool = vscode"),
        "stderr should include the .gitconfig snippet, got: {stderr}"
    );

    let log = fs::read_to_string(&code_log).unwrap_or_default();
    assert!(
        log.contains("arg: --wait"),
        "mock code should be called with --wait, got: {log}"
    );
    assert!(
        log.contains("arg: --diff"),
        "mock code should be called with --diff, got: {log}"
    );
}

#[test]
fn review_errors_when_no_difftool_and_no_vscode() {
    // When neither `diff.tool` nor `code` is available, `rumpel review`
    // must fail loudly rather than silently doing nothing.
    let repo = TestRepo::new();

    fs::write(repo.path().join("file.txt"), "initial\n").expect("Failed to write file");
    Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");
    create_commit(repo.path(), "Add file.txt");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "review-no-tool";

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
            "echo modified > file.txt && git add file.txt && git commit --no-verify -m m",
        ])
        .success()
        .expect("Failed to modify and commit in pod");

    // Do not install a mock `code`, and do not configure a difftool.
    let output = pod_command(&repo, &daemon)
        .args(["review", pod_name, "--yes"])
        .output()
        .expect("Failed to run rumpel review");

    assert!(
        !output.status.success(),
        "rumpel review should fail without a difftool or code"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no difftool configured") && stderr.contains("code"),
        "error should mention missing difftool and code, got: {stderr}"
    );
}

#[test]
fn review_nonexistent_pod_fails() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["review", "does-not-exist", "--yes"])
        .output()
        .expect("Failed to run rumpel review command");

    assert!(
        !output.status.success(),
        "rumpel review should fail for non-existent pod"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist"),
        "Error should say pod does not exist, got: {}",
        stderr
    );
}
