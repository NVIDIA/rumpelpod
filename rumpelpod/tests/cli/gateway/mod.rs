// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for git sync between host and pods.
//!
//! Tests verify that pods can push branches to refs/rumpelpod/ in the host repo
//! and that access control restricts cross-pod writes.

mod submodules;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use indoc::formatdoc;
use rumpelpod::CommandExt;
use sha2::{Digest, Sha256};

use crate::common::{
    create_commit, pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo,
    TEST_REPO_PATH,
};
use crate::executor::{self, ExecutorResources};

/// Get the list of pod refs (refs/rumpelpod/*) in a repository.
fn get_pod_refs(repo_path: &Path) -> Vec<String> {
    let output: String = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/rumpelpod/"])
        .current_dir(repo_path)
        .success()
        .expect("Failed to list pod refs")
        .try_into()
        .unwrap();
    output
        .lines()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Get the commit hash at a ref (full ref path like refs/rumpelpod/test@test).
fn get_pod_ref_commit(repo_path: &Path, ref_path: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", ref_path])
        .current_dir(repo_path)
        .success()
        .ok()
        .map(|b| String::try_from(b).unwrap().trim().to_string())
}

/// List pod refs in a host repository as full refnames under
/// `refs/rumpelpod/*`, filtered to entries whose short name contains
/// `@` (i.e. the per-branch `branch@pod` refs, not the primary alias).
fn get_host_pod_refs(repo_path: &Path) -> Vec<String> {
    let output: String = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/rumpelpod/"])
        .current_dir(repo_path)
        .success()
        .expect("Failed to list refs/rumpelpod refs")
        .try_into()
        .unwrap();
    output
        .lines()
        .filter(|s| !s.is_empty())
        .filter(|s| s.contains('@'))
        .map(String::from)
        .collect()
}

/// Get the commit hash at HEAD of a branch.
pub(super) fn get_branch_commit(repo_path: &Path, branch: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", branch])
        .current_dir(repo_path)
        .success()
        .ok()
        .map(|b| String::try_from(b).unwrap().trim().to_string())
}

/// Poll until a ref in the host repo reaches the expected commit.
/// The push from the container is asynchronous, so it may not have
/// landed by the time the triggering command returns.  No timeout
/// here -- the test harness kills us if we hang.
fn wait_for_ref_commit(repo_path: &Path, ref_path: &str, expected: &str) {
    loop {
        if get_pod_ref_commit(repo_path, ref_path).as_deref() == Some(expected) {
            return;
        }
        eprintln!("waiting for {ref_path} to reach {expected}...");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn wait_for_ref_commit_until(
    repo_path: &Path,
    ref_path: &str,
    expected: &str,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if get_pod_ref_commit(repo_path, ref_path).as_deref() == Some(expected) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        eprintln!("waiting for {ref_path} to reach {expected}...");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Create a new branch in the repo.
fn create_branch(repo_path: &Path, name: &str) {
    Command::new("git")
        .args(["checkout", "-b", name])
        .current_dir(repo_path)
        .success()
        .expect("git checkout -b failed");
}

/// Checkout an existing branch.
fn checkout_branch(repo_path: &Path, name: &str) {
    Command::new("git")
        .args(["checkout", name])
        .current_dir(repo_path)
        .success()
        .expect("git checkout failed");
}

#[test]
fn gateway_http_remotes_configured_in_container() {
    // Test that the container gets "host" and "rumpelpod" remotes pointing to the gateway
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "http-test", "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Check that remotes are configured
    let remotes_output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "http-test", "--", "git", "remote"])
        .success()
        .expect("Failed to get git remotes");

    let remotes = String::from_utf8_lossy(&remotes_output);

    assert!(
        remotes.contains("host"),
        "Container should have remote, got: {}",
        remotes
    );
    assert!(
        remotes.contains("rumpelpod"),
        "Container should have remote, got: {}",
        remotes
    );
}

#[test]
fn gateway_http_fetch_works_from_container() {
    // Test that a container can actually fetch from the gateway via HTTP
    let repo = TestRepo::new();

    // Create a branch with a commit before building the image
    create_branch(repo.path(), "test-branch");
    create_commit(repo.path(), "Test commit for fetch");
    let test_commit = get_branch_commit(repo.path(), "HEAD").unwrap();
    checkout_branch(repo.path(), "master");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod (this sets up gateway and pushes branches)
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fetch-test", "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Fetch from host remote inside the container
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "fetch-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to fetch from host remote");

    // Verify the fetched commit matches
    let fetched_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "fetch-test",
            "--",
            "git",
            "rev-parse",
            "host/test-branch",
        ])
        .success()
        .expect("Failed to get fetched commit");

    let fetched_commit = String::from_utf8_lossy(&fetched_commit_output)
        .trim()
        .to_string();

    assert_eq!(
        fetched_commit, test_commit,
        "Fetched commit should match host commit"
    );
}

#[test]
fn gateway_http_fetch_new_commits_after_create() {
    // Test that new commits made on host after container creation can be fetched
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod first
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "new-commits-test",
            "--",
            "echo",
            "setup",
        ])
        .success()
        .expect("Failed to run rumpel enter");

    // Initial fetch
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "new-commits-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to initial fetch");

    // Now create a new commit on host (after container exists)
    create_commit(repo.path(), "New commit after container creation");
    let new_commit = get_branch_commit(repo.path(), "HEAD").unwrap();

    // Fetch again - should get the new commit
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "new-commits-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to fetch new commits");

    // Verify we got the new commit
    let fetched_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "new-commits-test",
            "--",
            "git",
            "rev-parse",
            "host/master",
        ])
        .success()
        .expect("Failed to get fetched commit");

    let fetched_commit = String::from_utf8_lossy(&fetched_commit_output)
        .trim()
        .to_string();

    assert_eq!(
        fetched_commit, new_commit,
        "Should be able to fetch new commits made after container creation"
    );
}

#[test]
fn gateway_http_works_with_non_root_user() {
    // Test that git remotes work when running as a non-root user
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "user-test", "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Check that remotes are configured
    let remotes_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "user-test",
            "--",
            "git",
            "remote",
            "-v",
        ])
        .success()
        .expect("Failed to get git remotes");

    let remotes = String::from_utf8_lossy(&remotes_output);

    assert!(
        remotes.contains("host"),
        "Container should have remote, got: {}",
        remotes
    );

    // Fetch should work
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "user-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to fetch from host remote");
}

#[test]
fn gateway_pod_commit_triggers_push() {
    // Test that creating a commit in the pod triggers a push to refs/rumpelpod/ in the host
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "commit-push-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit in the pod
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
            "Pod commit",
        ])
        .success()
        .expect("Failed to create commit in pod");

    // Get the commit hash from the pod
    let pod_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod commit");

    let pod_commit = String::from_utf8_lossy(&pod_commit_output)
        .trim()
        .to_string();

    // The push from the pod's reference-transaction hook may still be
    // in flight, so poll until the host has the commit.
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &pod_commit);
}

#[test]
fn gateway_fresh_pod_establishes_ref() {
    // A pod that never commits still pushes its primary branch on the
    // first /events connection, so the host gets refs/rumpelpod/<pod>.
    // The push is now decided from local refs (the remote-tracking ref
    // is absent on a fresh pod) and runs asynchronously, so this guards
    // that a fresh pod's ref is still established without any commit
    // triggering the reference-transaction hook.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "fresh-pod-ref";

    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // The fresh pod's primary branch equals host/HEAD, so the
    // established ref must match the host repo's current HEAD.
    let host_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .success()
        .expect("rev-parse HEAD on host failed");
    let host_head = String::from_utf8_lossy(&host_head).trim().to_string();

    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &host_head);
    let shortcut_ref = format!("refs/rumpelpod/{pod_name}");
    wait_for_ref_commit(repo.path(), &shortcut_ref, &host_head);
}

#[test]
fn gateway_pod_push_works_from_new_branch() {
    // Test that pushing from a new branch in pod works
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "new-branch-push";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a new branch and commit in the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "checkout",
            "-b",
            "feature-from-pod",
        ])
        .success()
        .expect("Failed to create branch in pod");

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
            "Feature commit",
        ])
        .success()
        .expect("Failed to create commit in pod");

    // Get the commit hash from the pod
    let pod_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod commit");

    let pod_commit = String::from_utf8_lossy(&pod_commit_output)
        .trim()
        .to_string();

    // Check that the host has the pod ref
    let expected_ref = format!("refs/rumpelpod/feature-from-pod@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &pod_commit);
}

#[test]
fn gateway_multiple_pods_push_independently() {
    // Test that multiple pods can push to the host without conflicts
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch two pods
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "pod-a", "--", "echo", "setup"])
        .success()
        .expect("Failed to run pod-a enter");

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "pod-b", "--", "echo", "setup"])
        .success()
        .expect("Failed to run pod-b enter");

    // Create commits in both pods on the same branch name
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-a",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Commit from A",
        ])
        .success()
        .expect("Failed to create commit in pod-a");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-b",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Commit from B",
        ])
        .success()
        .expect("Failed to create commit in pod-b");

    // Get commit hashes
    let commit_a_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-a",
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod-a commit");
    let commit_a = String::from_utf8_lossy(&commit_a_output).trim().to_string();

    let commit_b_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-b",
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod-b commit");
    let commit_b = String::from_utf8_lossy(&commit_b_output).trim().to_string();

    // Check that the host has both pod refs with correct commits
    wait_for_ref_commit(repo.path(), "refs/rumpelpod/pod-a@pod-a", &commit_a);
    wait_for_ref_commit(repo.path(), "refs/rumpelpod/pod-b@pod-b", &commit_b);

    // The commits should be different
    assert_ne!(
        commit_a, commit_b,
        "Commits from different pods should be different"
    );
}

#[test]
fn gateway_pod_amend_triggers_push() {
    // Test that amending a commit in the pod triggers a push to refs/rumpelpod/
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "amend-push-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit in the pod
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
            "Original commit",
        ])
        .success()
        .expect("Failed to create commit in pod");

    let original_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get original commit");
    let original_commit = String::from_utf8_lossy(&original_commit_output)
        .trim()
        .to_string();

    // Wait for original commit to land so we can verify it changes
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &original_commit);

    // Amend the commit
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "commit",
            "--no-verify",
            "--amend",
            "--allow-empty",
            "-m",
            "Amended commit",
        ])
        .success()
        .expect("Failed to amend commit in pod");

    let amended_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get amended commit");
    let amended_commit = String::from_utf8_lossy(&amended_commit_output)
        .trim()
        .to_string();

    // Commits should differ
    assert_ne!(
        original_commit, amended_commit,
        "Amended commit should have different hash"
    );

    // Check that the host has the amended commit
    wait_for_ref_commit(repo.path(), &expected_ref, &amended_commit);
}

#[test]
fn gateway_pod_cannot_push_to_other_pod_namespace() {
    // Test that pod-a cannot push to pod-b's namespace
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod-a
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "pod-a", "--", "echo", "setup"])
        .success()
        .expect("Failed to run pod-a enter");

    // Launch pod-b and create a commit so it has a ref in the host
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "pod-b", "--", "echo", "setup"])
        .success()
        .expect("Failed to run pod-b enter");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-b",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Commit from B",
        ])
        .success()
        .expect("Failed to create commit in pod-b");

    // Get pod-b's commit
    let commit_b_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-b",
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod-b commit");
    let commit_b = String::from_utf8_lossy(&commit_b_output).trim().to_string();

    // Verify pod-b's ref exists in host
    wait_for_ref_commit(repo.path(), "refs/rumpelpod/pod-b@pod-b", &commit_b);

    // Now try to have pod-a push directly to pod-b's namespace
    // This should fail because of access control
    let result = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-a",
            "--",
            "git",
            "push",
            "rumpelpod",
            "HEAD:refs/rumpelpod/pod-b@pod-b",
            "--force",
        ])
        .output()
        .expect("Failed to execute push command");

    // The push should fail
    assert!(
        !result.status.success(),
        "Push to another pod's namespace should fail, but it succeeded"
    );

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("pod 'pod-a' cannot push to") || stderr.contains("cannot push"),
        "Error message should mention access control, got: {}",
        stderr
    );

    // Verify pod-b's ref still has its original commit (not overwritten)
    let pod_b_commit_after = get_pod_ref_commit(repo.path(), "refs/rumpelpod/pod-b@pod-b");
    assert_eq!(
        pod_b_commit_after,
        Some(commit_b),
        "pod-b's ref should not have been modified"
    );
}

#[test]
fn gateway_pod_cannot_push_to_host_namespace() {
    // Test that a pod cannot push to refs/heads/ (host branches)
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Get master commit before the attack
    let host_commit_before = get_branch_commit(repo.path(), "master");

    // Create a commit in pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "test",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Malicious commit",
        ])
        .success()
        .expect("Failed to create commit in pod");

    // Try to push to refs/heads/master - should be rejected
    let result = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "test",
            "--",
            "git",
            "push",
            "rumpelpod",
            "HEAD:refs/heads/master",
            "--force",
        ])
        .output()
        .expect("Failed to execute push command");

    // The push should fail
    assert!(
        !result.status.success(),
        "Push to host namespace should fail, but it succeeded"
    );

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("cannot push to") || stderr.contains("rumpelpod"),
        "Error message should mention access control, got: {}",
        stderr
    );

    // Verify master still has its original commit
    let host_commit_after = get_branch_commit(repo.path(), "master");
    assert_eq!(
        host_commit_after, host_commit_before,
        "master should not have been modified"
    );
}

#[test]
fn gateway_pod_can_push_to_own_namespace() {
    // Test that a pod can push to its own namespace (positive test)
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "my-pod";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a new branch and commit
    pod_command(&repo, &daemon)
        .args([
            "enter", "--create", pod_name, "--", "git", "checkout", "-b", "feature",
        ])
        .success()
        .expect("Failed to create branch");

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
            "Feature commit",
        ])
        .success()
        .expect("Failed to create commit");

    // Get the commit hash
    let commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get commit");
    let commit = String::from_utf8_lossy(&commit_output).trim().to_string();

    // Manually push to our own namespace with explicit refspec
    let explicit_push = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "push",
            "rumpelpod",
            &format!("HEAD:refs/rumpelpod/feature@{pod_name}"),
            "--force",
        ])
        .output()
        .expect("Failed to execute push command");

    assert!(
        explicit_push.status.success(),
        "Push to own namespace should succeed, but failed: {}",
        String::from_utf8_lossy(&explicit_push.stderr)
    );

    // Verify the ref exists with our commit
    let expected_ref = format!("refs/rumpelpod/feature@{pod_name}");
    let host_commit = get_pod_ref_commit(repo.path(), &expected_ref);

    assert_eq!(
        host_commit,
        Some(commit),
        "Host should have our commit on {expected_ref}"
    );
}

#[test]
fn gateway_pod_reset_triggers_push() {
    // Test that resetting in the pod triggers a push to refs/rumpelpod/
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "reset-push-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create two commits in the pod
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
            "Commit 1",
        ])
        .success()
        .expect("Failed to create commit 1");

    let commit1_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get commit1");
    let commit1 = String::from_utf8_lossy(&commit1_output).trim().to_string();

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
            "Commit 2",
        ])
        .success()
        .expect("Failed to create commit 2");

    let commit2_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get commit2");
    let commit2 = String::from_utf8_lossy(&commit2_output).trim().to_string();

    // Wait for commit2 to land so we know the ref exists
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &commit2);

    // Reset back to commit1 (no new commit created)
    pod_command(&repo, &daemon)
        .args([
            "enter", "--create", pod_name, "--", "git", "reset", "--hard", &commit1,
        ])
        .success()
        .expect("Failed to reset in pod");

    // Host should now have commit1 (the pod's hook pushes on reset)
    wait_for_ref_commit(repo.path(), &expected_ref, &commit1);
}

#[test]
fn gateway_pod_branch_creation_triggers_push() {
    // Test that creating a new branch (without making a commit) in the pod triggers a push
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "branch-create-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Verify ref doesn't exist yet
    let expected_ref = format!("refs/rumpelpod/new-feature@{pod_name}");
    let refs_before = get_pod_refs(repo.path());
    assert!(
        !refs_before.contains(&expected_ref),
        "Ref should not exist yet"
    );

    // Get the current commit
    let current_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get current commit");
    let current_commit = String::from_utf8_lossy(&current_commit_output)
        .trim()
        .to_string();

    // Create a new branch WITHOUT making a commit
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "checkout",
            "-b",
            "new-feature",
        ])
        .success()
        .expect("Failed to create branch in pod");

    // Host should now have the ref (the pod's hook pushes on branch creation)
    wait_for_ref_commit(repo.path(), &expected_ref, &current_commit);
}

#[test]
fn gateway_pod_push_syncs_to_host_remote_ref() {
    // Test that when a pod pushes, it lands as refs/rumpelpod/<pod>@<pod>
    // on the host.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "sync-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit in the pod
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
            "Pod commit",
        ])
        .success()
        .expect("Failed to create commit in pod");

    // Get the commit hash
    let commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get commit");
    let commit = String::from_utf8_lossy(&commit_output).trim().to_string();

    // Wait for pod ref to land in host
    let expected_pod_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_pod_ref, &commit);
}

#[test]
fn gateway_pod_branch_sync_to_host() {
    // Test that creating a new branch in pod syncs to host remote refs
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "branch-sync-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a new branch in pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "checkout",
            "-b",
            "feature-x",
        ])
        .success()
        .expect("Failed to create branch in pod");

    // Get the commit so we can wait for it
    let commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get commit");
    let commit = String::from_utf8_lossy(&commit_output).trim().to_string();

    // Wait for pod ref to land
    let expected_pod_ref = format!("refs/rumpelpod/feature-x@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_pod_ref, &commit);
}

#[test]
fn gateway_multiple_pods_sync_to_host() {
    // Test that multiple pods can sync independently to host remote refs
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create first pod and commit
    let pod1 = "multi-sync-1";
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod1,
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Commit from pod 1",
        ])
        .success()
        .expect("Failed to create commit in pod 1");

    let commit1_output = pod_command(&repo, &daemon)
        .args(["enter", "--create", pod1, "--", "git", "rev-parse", "HEAD"])
        .success()
        .expect("Failed to get commit 1");
    let commit1 = String::from_utf8_lossy(&commit1_output).trim().to_string();

    // Create second pod and commit
    let pod2 = "multi-sync-2";
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod2,
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Commit from pod 2",
        ])
        .success()
        .expect("Failed to create commit in pod 2");

    let commit2_output = pod_command(&repo, &daemon)
        .args(["enter", "--create", pod2, "--", "git", "rev-parse", "HEAD"])
        .success()
        .expect("Failed to get commit 2");
    let commit2 = String::from_utf8_lossy(&commit2_output).trim().to_string();

    // Wait for pod refs to land
    wait_for_ref_commit(
        repo.path(),
        &format!("refs/rumpelpod/{pod1}@{pod1}"),
        &commit1,
    );
    wait_for_ref_commit(
        repo.path(),
        &format!("refs/rumpelpod/{pod2}@{pod2}"),
        &commit2,
    );

    // Both pod refs should appear in the host's refs/rumpelpod listing.
    let expected_ref1 = format!("refs/rumpelpod/{pod1}@{pod1}");
    let expected_ref2 = format!("refs/rumpelpod/{pod2}@{pod2}");
    let refs = get_host_pod_refs(repo.path());

    assert!(
        refs.contains(&expected_ref1),
        "Host should list {expected_ref1}, got: {refs:?}"
    );
    assert!(
        refs.contains(&expected_ref2),
        "Host should list {expected_ref2}, got: {refs:?}"
    );
}

#[test]
fn gateway_pod_reset_syncs_to_host_via_force_push() {
    // Test that resetting in pod (requiring force push) syncs to host refs
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "force-push-test";

    // Launch pod and create two commits
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
            "First commit",
        ])
        .success()
        .expect("Failed to create first commit");

    let commit1_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get commit1");
    let commit1 = String::from_utf8_lossy(&commit1_output).trim().to_string();

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
            "Second commit",
        ])
        .success()
        .expect("Failed to create second commit");

    let commit2_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get commit2");
    let commit2 = String::from_utf8_lossy(&commit2_output).trim().to_string();

    // Verify host has commit2
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &commit2);

    // Reset back to commit1 (requires force push to update host ref)
    pod_command(&repo, &daemon)
        .args([
            "enter", "--create", pod_name, "--", "git", "reset", "--hard", &commit1,
        ])
        .success()
        .expect("Failed to reset in pod");

    // Wait for force push to land
    wait_for_ref_commit(repo.path(), &expected_ref, &commit1);
}

#[test]
fn gateway_host_head_available_in_pod() {
    // Test that pod can access host/HEAD as a remote-tracking ref.
    // The HTTP server updates refs/rumpelpod/host-head lazily (on fetch),
    // and the pod's host remote maps it to refs/remotes/host/HEAD.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Get the current HEAD commit
    let head_commit = get_branch_commit(repo.path(), "HEAD").unwrap();

    // Launch pod and fetch host refs
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-head-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to fetch host refs");

    // Verify pod can resolve host/HEAD
    let pod_head_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-head-test",
            "--",
            "git",
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to resolve host/HEAD in pod");

    let pod_head_commit = String::from_utf8_lossy(&pod_head_output).trim().to_string();

    assert_eq!(
        pod_head_commit, head_commit,
        "Pod should be able to resolve host/HEAD to the host's current commit"
    );
}

#[test]
fn gateway_host_head_updates_on_fetch() {
    // Test that host/HEAD is updated when the pod fetches after a new
    // host commit.  The HTTP server updates refs/rumpelpod/host-head
    // lazily on each request, so the pod sees the new HEAD after fetching.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod and do initial fetch
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "head-commit-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to run initial fetch");

    // Get initial host/HEAD commit from pod
    let initial_head_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "head-commit-test",
            "--",
            "git",
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to resolve initial host/HEAD");
    let initial_head = String::from_utf8_lossy(&initial_head_output)
        .trim()
        .to_string();

    // Make a new commit on host
    create_commit(repo.path(), "Test commit for HEAD update");
    let new_commit = get_branch_commit(repo.path(), "HEAD").unwrap();

    // Fetch again - should pick up updated host-head ref
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "head-commit-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to fetch after host commit");

    let updated_head_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "head-commit-test",
            "--",
            "git",
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to resolve updated host/HEAD");
    let updated_head = String::from_utf8_lossy(&updated_head_output)
        .trim()
        .to_string();

    assert_ne!(
        initial_head, updated_head,
        "host/HEAD should be updated after commit and fetch"
    );
    assert_eq!(
        updated_head, new_commit,
        "host/HEAD should point to the new commit"
    );
}

#[test]
fn gateway_host_head_updates_on_branch_switch() {
    // Test that host/HEAD is updated when switching branches, visible
    // to the pod after fetching.
    let repo = TestRepo::new();

    // Create a feature branch with different commit
    create_branch(repo.path(), "feature");
    create_commit(repo.path(), "Feature commit");
    let feature_commit = get_branch_commit(repo.path(), "HEAD").unwrap();

    checkout_branch(repo.path(), "master");
    let master_commit = get_branch_commit(repo.path(), "HEAD").unwrap();

    // Sanity check: different commits
    assert_ne!(master_commit, feature_commit);

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod (host is on master) and fetch
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "branch-switch-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to run rumpel enter");

    // Verify host/HEAD points to master commit
    let pod_head_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "branch-switch-test",
            "--",
            "git",
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to resolve host/HEAD");
    let pod_head = String::from_utf8_lossy(&pod_head_output).trim().to_string();
    assert_eq!(
        pod_head, master_commit,
        "host/HEAD should point to master commit initially"
    );

    // Switch to feature branch on host
    checkout_branch(repo.path(), "feature");

    // Pod should see the updated HEAD after fetch
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "branch-switch-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to fetch after branch switch");

    let pod_head_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "branch-switch-test",
            "--",
            "git",
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to resolve host/HEAD after switch");

    let pod_head = String::from_utf8_lossy(&pod_head_output).trim().to_string();

    assert_eq!(
        pod_head, feature_commit,
        "Pod host/HEAD should reflect the branch switch"
    );
}

#[test]
fn gateway_host_head_works_in_detached_state() {
    // Test that host/HEAD works correctly when host is in detached HEAD state
    let repo = TestRepo::new();

    // Create a few commits to have history
    create_commit(repo.path(), "First commit");
    let first_commit = get_branch_commit(repo.path(), "HEAD").unwrap();

    create_commit(repo.path(), "Second commit");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Checkout first commit in detached HEAD state
    Command::new("git")
        .args(["checkout", &first_commit])
        .current_dir(repo.path())
        .success()
        .expect("git checkout (detached) failed");

    // Launch pod and fetch - should see the detached HEAD commit
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "detached-head-test",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to fetch in detached HEAD state");

    let pod_head_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "detached-head-test",
            "--",
            "git",
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to resolve host/HEAD");

    let pod_head = String::from_utf8_lossy(&pod_head_output).trim().to_string();

    assert_eq!(
        pod_head, first_commit,
        "Pod host/HEAD should resolve to the detached HEAD commit"
    );
}

#[test]
fn gateway_setup_works_with_detached_head() {
    // Detach HEAD before the first `rumpel enter` so that setup runs
    // with zero local branches.  The pod should still be created and
    // host/HEAD should be resolvable.
    let repo = TestRepo::new();

    create_commit(repo.path(), "First commit");
    let commit = get_branch_commit(repo.path(), "HEAD").unwrap();

    // Detach HEAD and delete the only branch.
    Command::new("git")
        .args(["checkout", "--detach"])
        .current_dir(repo.path())
        .success()
        .expect("git checkout --detach failed");
    Command::new("git")
        .args(["branch", "-D", "master"])
        .current_dir(repo.path())
        .success()
        .expect("git branch -D master failed");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "detached-setup",
            "--",
            "git",
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to run rumpel enter on detached HEAD repo");

    // Pod should be able to resolve host/HEAD to the detached commit
    let pod_head_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "detached-setup",
            "--",
            "git",
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to resolve host/HEAD");

    let pod_head = String::from_utf8_lossy(&pod_head_output).trim().to_string();
    assert_eq!(
        pod_head, commit,
        "host/HEAD should be resolvable even when repo has no branches"
    );
}

#[test]
fn gateway_primary_branch_alias_works_on_host() {
    // Test that refs/rumpelpod/<name> resolves to the same commit as
    // refs/rumpelpod/<name>@<name>.  The post-receive hook creates this
    // convenience shortcut for the primary branch.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "alias-test";

    // Launch pod - it automatically creates and checks out a branch named after
    // the pod (the "primary branch")
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to launch pod");

    // Create a commit on the primary branch
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
            "test commit for alias",
        ])
        .success()
        .expect("Failed to create commit in pod");

    // Get commit from pod
    let commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod HEAD");
    let pod_commit = String::from_utf8_lossy(&commit_output).trim().to_string();

    // Wait for the full ref to land
    let full_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &full_ref, &pod_commit);

    // Verify both refs point to the same commit
    let alias_ref = format!("refs/rumpelpod/{pod_name}");

    let full_ref_commit = get_pod_ref_commit(repo.path(), &full_ref);
    let alias_ref_commit = get_pod_ref_commit(repo.path(), &alias_ref);

    assert_eq!(
        full_ref_commit,
        Some(pod_commit.clone()),
        "Full ref {full_ref} should point to pod commit"
    );
    assert_eq!(
        alias_ref_commit,
        Some(pod_commit),
        "Alias ref {alias_ref} should point to same commit as full ref"
    );
}

#[test]
fn gateway_alias_deleted_with_pod() {
    // Test that alias ref is removed when pod is deleted
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "delete-alias-test";

    // Launch pod - it automatically creates and checks out the primary branch
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to launch pod");

    // Create a commit on the primary branch
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
            "test commit",
        ])
        .success()
        .expect("Failed to create commit in pod");

    // Get commit for polling
    let commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod HEAD");
    let pod_commit = String::from_utf8_lossy(&commit_output).trim().to_string();

    // Wait for refs to land
    let full_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    let alias_ref = format!("refs/rumpelpod/{pod_name}");
    wait_for_ref_commit(repo.path(), &full_ref, &pod_commit);

    // Verify refs exist
    assert!(
        get_pod_ref_commit(repo.path(), &full_ref).is_some(),
        "Full ref should exist before deletion"
    );
    assert!(
        get_pod_ref_commit(repo.path(), &alias_ref).is_some(),
        "Alias ref should exist before deletion"
    );

    // Delete the pod (--wait so refs are cleaned up before checking)
    pod_command(&repo, &daemon)
        .args(["delete", "--wait", "--force", pod_name])
        .success()
        .expect("Failed to delete pod");

    // Verify refs are removed
    assert!(
        get_pod_ref_commit(repo.path(), &full_ref).is_none(),
        "Full ref should be removed after deletion"
    );
    assert!(
        get_pod_ref_commit(repo.path(), &alias_ref).is_none(),
        "Alias ref should be removed after deletion"
    );
}

#[test]
fn gateway_alias_does_not_conflict_with_branches() {
    // Test that pod `alice` creating branch `bob` does not affect pod `bob`'s alias
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch pod `bob` - it automatically creates and checks out its primary branch
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "bob", "--", "echo", "setup"])
        .success()
        .expect("Failed to launch bob pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "bob",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "bob commit",
        ])
        .success()
        .expect("Failed to create commit in bob pod");

    let bob_commit_output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "bob", "--", "git", "rev-parse", "HEAD"])
        .success()
        .expect("Failed to get bob HEAD");
    let bob_commit = String::from_utf8_lossy(&bob_commit_output)
        .trim()
        .to_string();

    // Wait for bob's refs to land
    wait_for_ref_commit(repo.path(), "refs/rumpelpod/bob@bob", &bob_commit);

    // Create pod `alice` and create a branch named `bob` with different commit
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "alice",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "alice commit",
        ])
        .success()
        .expect("Failed to create commit in alice pod");

    pod_command(&repo, &daemon)
        .args([
            "enter", "--create", "alice", "--", "git", "checkout", "-b", "bob",
        ])
        .success()
        .expect("Failed to create bob branch in alice pod");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "alice",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "alice bob-branch commit",
        ])
        .success()
        .expect("Failed to create commit on bob branch in alice pod");

    let alice_bob_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "alice",
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get alice bob-branch HEAD");
    let alice_bob_commit = String::from_utf8_lossy(&alice_bob_commit_output)
        .trim()
        .to_string();

    // Wait for alice's bob branch to land
    wait_for_ref_commit(repo.path(), "refs/rumpelpod/bob@alice", &alice_bob_commit);

    // Verify refs/rumpelpod/bob alias still points to bob's primary branch, not alice's
    let bob_alias_commit = get_pod_ref_commit(repo.path(), "refs/rumpelpod/bob");
    let bob_full_commit = get_pod_ref_commit(repo.path(), "refs/rumpelpod/bob@bob");
    let alice_bob_branch_commit = get_pod_ref_commit(repo.path(), "refs/rumpelpod/bob@alice");

    assert_ne!(
        bob_commit, alice_bob_commit,
        "Bob and alice's bob-branch should have different commits"
    );
    assert_eq!(
        bob_alias_commit,
        Some(bob_commit.clone()),
        "refs/rumpelpod/bob alias should point to bob's primary branch"
    );
    assert_eq!(
        bob_full_commit,
        Some(bob_commit),
        "refs/rumpelpod/bob@bob should point to bob's commit"
    );
    assert_eq!(
        alice_bob_branch_commit,
        Some(alice_bob_commit),
        "refs/rumpelpod/bob@alice should point to alice's bob-branch commit"
    );
}

#[test]
fn gateway_non_primary_branches_have_no_alias() {
    // Test that non-primary branches (feature@pod) don't get an alias
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "no-alias-test";

    // Launch pod - it automatically creates and checks out its primary branch
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to launch pod");

    // Create initial commit on primary branch
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
            "primary commit",
        ])
        .success()
        .expect("Failed to create commit on primary branch");

    let primary_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get primary commit");
    let primary_commit = String::from_utf8_lossy(&primary_commit_output)
        .trim()
        .to_string();

    // Wait for primary ref to land
    let primary_full_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &primary_full_ref, &primary_commit);

    // Create a feature branch
    pod_command(&repo, &daemon)
        .args([
            "enter", "--create", pod_name, "--", "git", "checkout", "-b", "feature",
        ])
        .success()
        .expect("Failed to create feature branch");

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
            "feature commit",
        ])
        .success()
        .expect("Failed to create commit on feature branch");

    let feature_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get feature commit");
    let feature_commit = String::from_utf8_lossy(&feature_commit_output)
        .trim()
        .to_string();

    // Wait for feature ref to land
    let feature_ref = format!("refs/rumpelpod/feature@{pod_name}");
    wait_for_ref_commit(repo.path(), &feature_ref, &feature_commit);

    // Verify feature@pod_name exists but refs/rumpelpod/feature alias does not
    assert!(
        get_pod_ref_commit(repo.path(), &feature_ref).is_some(),
        "refs/rumpelpod/feature@{pod_name} should exist"
    );
    assert!(
        get_pod_ref_commit(repo.path(), "refs/rumpelpod/feature").is_none(),
        "refs/rumpelpod/feature alias should not exist (only primary branches get aliases)"
    );

    // Verify primary branch alias still works
    let primary_alias_ref = format!("refs/rumpelpod/{pod_name}");
    let primary_alias_commit = get_pod_ref_commit(repo.path(), &primary_alias_ref);
    let primary_full_commit = get_pod_ref_commit(repo.path(), &primary_full_ref);
    assert!(
        primary_alias_commit.is_some(),
        "Primary branch alias should exist"
    );
    assert_eq!(
        primary_alias_commit, primary_full_commit,
        "Primary branch alias should point to same commit as full ref"
    );
}

#[test]
fn pod_has_branch_named_after_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Enter pod and check the current branch name
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "my-pod",
            "--",
            "git",
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ])
        .success()
        .expect("rumpel enter failed");

    let branch = String::from_utf8_lossy(&stdout).trim().to_string();
    assert_eq!(
        branch, "my-pod",
        "Expected branch 'my-pod' to be checked out, got '{branch}'"
    );
}

#[test]
fn pod_branch_points_to_host_head() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Get the host's HEAD commit
    let host_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .success()
        .expect("Failed to get host HEAD");
    let host_head = String::from_utf8_lossy(&host_head).trim().to_string();

    // Enter pod and get its HEAD commit
    let pod_head = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "test-branch",
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("rumpel enter failed");
    let pod_head = String::from_utf8_lossy(&pod_head).trim().to_string();

    assert_eq!(
        pod_head, host_head,
        "Pod HEAD ({pod_head}) should match host HEAD ({host_head})"
    );
}

#[test]
fn pod_has_host_remote_refs() {
    let repo = TestRepo::new();

    // Create a branch on the host before building the image
    Command::new("git")
        .args(["checkout", "-b", "feature-branch"])
        .current_dir(repo.path())
        .success()
        .expect("Failed to create feature branch");

    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "Feature commit"])
        .current_dir(repo.path())
        .success()
        .expect("Failed to commit on feature branch");

    // Switch back to the default branch
    Command::new("git")
        .args(["checkout", "-"])
        .current_dir(repo.path())
        .success()
        .expect("Failed to switch back");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Enter pod and list remote refs
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "test-refs",
            "--",
            "git",
            "branch",
            "-r",
        ])
        .success()
        .expect("rumpel enter failed");

    let refs = String::from_utf8_lossy(&stdout);

    // Should have host/HEAD and host/feature-branch
    assert!(
        refs.contains("host/HEAD"),
        "Expected host/HEAD in remote refs: {refs}"
    );
    assert!(
        refs.contains("host/feature-branch"),
        "Expected host/feature-branch in remote refs: {refs}"
    );
}

#[test]
fn pod_host_fetch_does_not_include_pod_refs() {
    // git fetch host in a pod should only get refs/heads/* and
    // refs/rumpelpod/host-head from the host.  It must NOT pick up
    // refs/rumpelpod/<branch>@<pod> or refs/remotes/*.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let pod_name = "fetch-scope";

    // Enter pod and make a commit so pod refs exist on the host.
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
            "pod commit",
        ])
        .success()
        .expect("pod commit failed");

    // Wait for the pod ref to land on the host.
    let commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("rev-parse failed");
    let commit = String::from_utf8_lossy(&commit_output).trim().to_string();
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &commit);

    // Now fetch host inside the pod and list all remote refs.
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "git", "fetch", "host"])
        .success()
        .expect("git fetch host failed");

    let refs_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "for-each-ref",
            "--format=%(refname)",
            "refs/remotes/host/",
        ])
        .success()
        .expect("for-each-ref failed");
    let refs_str = String::from_utf8_lossy(&refs_output);

    let actual: BTreeSet<_> = refs_str.lines().filter(|s| !s.is_empty()).collect();
    let expected = BTreeSet::from(["refs/remotes/host/HEAD", "refs/remotes/host/master"]);

    assert_eq!(actual, expected);
}

#[test]
fn pod_rumpelpod_fetch_sees_other_pods() {
    // git fetch rumpelpod in a pod should see other pods' branches
    // (refs/rumpelpod/*) mapped as rumpelpod/* remote-tracking refs.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create two pods with commits.
    for name in &["pod-alpha", "pod-beta"] {
        pod_command(&repo, &daemon)
            .args([
                "enter",
                "--create",
                name,
                "--",
                "git",
                "commit",
                "--no-verify",
                "--allow-empty",
                "-m",
                "commit",
            ])
            .success()
            .unwrap_or_else(|_| panic!("commit in {name} failed"));
    }

    // Wait for both pod refs to land on the host.
    for name in &["pod-alpha", "pod-beta"] {
        let output = pod_command(&repo, &daemon)
            .args(["enter", "--create", name, "--", "git", "rev-parse", "HEAD"])
            .success()
            .unwrap_or_else(|_| panic!("rev-parse in {name} failed"));
        let commit = String::from_utf8_lossy(&output).trim().to_string();
        let expected = format!("refs/rumpelpod/{name}@{name}");
        wait_for_ref_commit(repo.path(), &expected, &commit);
    }

    // Fetch rumpelpod from inside pod-alpha.
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-alpha",
            "--",
            "git",
            "fetch",
            "rumpelpod",
        ])
        .success()
        .expect("git fetch rumpelpod failed");

    let refs_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "pod-alpha",
            "--",
            "git",
            "for-each-ref",
            "--format=%(refname)",
            "refs/remotes/rumpelpod/",
        ])
        .success()
        .expect("for-each-ref failed");
    let refs_str = String::from_utf8_lossy(&refs_output);

    let actual: BTreeSet<_> = refs_str.lines().filter(|s| !s.is_empty()).collect();
    let expected = BTreeSet::from([
        // Primary shortcuts.
        "refs/remotes/rumpelpod/pod-alpha",
        "refs/remotes/rumpelpod/pod-beta",
        // Pod branches (branch@pod for each branch the pod pushed).
        "refs/remotes/rumpelpod/pod-alpha@pod-alpha",
        "refs/remotes/rumpelpod/pod-beta@pod-beta",
        // Each pod also pushes its master branch (from the initial clone).
        "refs/remotes/rumpelpod/master@pod-alpha",
        "refs/remotes/rumpelpod/master@pod-beta",
        // host-head lands here because the fetch refspec covers all of
        // refs/rumpelpod/*.
        "refs/remotes/rumpelpod/host-head",
    ]);

    assert_eq!(actual, expected);
}

#[test]
fn pod_branch_is_different_from_host_branches() {
    // The pod branch should be a new branch, not one that already exists on host
    let repo = TestRepo::new();

    // Create a branch named "existing" on host
    Command::new("git")
        .args(["checkout", "-b", "existing"])
        .current_dir(repo.path())
        .success()
        .expect("Failed to create existing branch");

    Command::new("git")
        .args(["checkout", "-"])
        .current_dir(repo.path())
        .success()
        .expect("Failed to switch back");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // The pod named "existing" should still have its own branch, separate
    // from the host's "existing" branch (which becomes host/existing)
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "existing",
            "--",
            "git",
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ])
        .success()
        .expect("rumpel enter failed");

    let branch = String::from_utf8_lossy(&stdout).trim().to_string();
    assert_eq!(
        branch, "existing",
        "Expected branch 'existing' to be checked out, got '{branch}'"
    );

    // Check that both the local branch and remote ref exist
    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "--create", "existing", "--", "git", "branch", "-a"])
        .success()
        .expect("rumpel enter failed");

    let branches = String::from_utf8_lossy(&stdout);
    assert!(
        branches.contains("* existing"),
        "Expected '* existing' (checked out local branch) in: {branches}"
    );
    assert!(
        branches.contains("remotes/host/existing"),
        "Expected 'remotes/host/existing' in: {branches}"
    );
}

#[test]
fn re_entering_pod_preserves_branch() {
    // Re-entering a pod should not reset the branch or checkout
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // First enter - creates the pod and branch
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "persist-test", "--", "echo", "first"])
        .success()
        .expect("first rumpel enter failed");

    // Make a commit in the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "persist-test",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Pod commit",
        ])
        .success()
        .expect("pod commit failed");

    // Get the pod HEAD after the commit
    let head_after_commit = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "persist-test",
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("git rev-parse failed");
    let head_after_commit = String::from_utf8_lossy(&head_after_commit)
        .trim()
        .to_string();

    // Re-enter the pod and check HEAD is still at the same commit
    let head_after_reenter = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "persist-test",
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("git rev-parse failed after re-enter");
    let head_after_reenter = String::from_utf8_lossy(&head_after_reenter)
        .trim()
        .to_string();

    assert_eq!(
        head_after_commit, head_after_reenter,
        "Re-entering pod should preserve HEAD commit"
    );

    // Check we're still on the same branch
    let branch = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "persist-test",
            "--",
            "git",
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ])
        .success()
        .expect("git rev-parse --abbrev-ref failed");
    let branch = String::from_utf8_lossy(&branch).trim().to_string();

    assert_eq!(
        branch, "persist-test",
        "Re-entering pod should preserve branch name"
    );
}

/// Get the upstream tracking branch for a local branch in the pod.
fn get_pod_upstream(
    repo: &TestRepo,
    daemon: &TestDaemon,
    pod_name: &str,
    branch: &str,
) -> Option<String> {
    pod_command(repo, daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "--abbrev-ref",
            &format!("{branch}@{{upstream}}"),
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[test]
fn pod_primary_branch_has_upstream_when_host_on_branch() {
    // When the host is on a branch, the pod's primary branch should track it
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Host is on "master" branch by default (from TestRepo::new)
    let pod_name = "upstream-test";

    // Enter pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("rumpel enter failed");

    // Check that the primary branch tracks host/master
    let upstream = get_pod_upstream(&repo, &daemon, pod_name, pod_name);
    assert_eq!(
        upstream,
        Some("host/master".to_string()),
        "Primary branch should track host/master"
    );
}

#[test]
fn pod_primary_branch_tracks_host_feature_branch() {
    // When the host is on a feature branch, the pod should track that branch
    let repo = TestRepo::new();

    // Create and switch to a feature branch
    create_branch(repo.path(), "feature-xyz");
    create_commit(repo.path(), "Feature commit");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "feature-upstream-test";

    // Enter pod while host is on feature-xyz
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("rumpel enter failed");

    // Check that the primary branch tracks host/feature-xyz
    let upstream = get_pod_upstream(&repo, &daemon, pod_name, pod_name);
    assert_eq!(
        upstream,
        Some("host/feature-xyz".to_string()),
        "Primary branch should track host/feature-xyz"
    );
}

#[test]
fn pod_primary_branch_has_no_upstream_when_host_detached() {
    // When the host is in detached HEAD state, the pod should have no upstream
    let repo = TestRepo::new();

    // Create a commit and get its hash
    create_commit(repo.path(), "Test commit");
    let commit = get_branch_commit(repo.path(), "HEAD").unwrap();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Now detach HEAD by checking out the commit directly
    Command::new("git")
        .args(["checkout", &commit])
        .current_dir(repo.path())
        .success()
        .expect("git checkout (detached) failed");

    let pod_name = "detached-upstream-test";

    // Enter pod while host is detached
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("rumpel enter failed");

    // Check that the primary branch has no upstream
    let upstream = get_pod_upstream(&repo, &daemon, pod_name, pod_name);
    assert!(
        upstream.is_none(),
        "Primary branch should have no upstream when host is detached, got: {upstream:?}"
    );
}

#[test]
fn pod_re_entry_does_not_change_upstream() {
    // Re-entering a pod should not change the upstream, even if host branch changed
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "upstream-preserve-test";

    // First entry while on master
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "first entry"])
        .success()
        .expect("first rumpel enter failed");

    // Verify upstream is host/master
    let upstream_before = get_pod_upstream(&repo, &daemon, pod_name, pod_name);
    assert_eq!(
        upstream_before,
        Some("host/master".to_string()),
        "Primary branch should track host/master initially"
    );

    // Switch host to a different branch
    create_branch(repo.path(), "other-branch");

    // Re-enter pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "second entry"])
        .success()
        .expect("second rumpel enter failed");

    // Verify upstream is still host/master (not changed to host/other-branch)
    let upstream_after = get_pod_upstream(&repo, &daemon, pod_name, pod_name);
    assert_eq!(
        upstream_after,
        Some("host/master".to_string()),
        "Primary branch upstream should not change on re-entry"
    );
}

#[test]
fn pod_unsafe_host_network_mode() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    // Overwrite devcontainer.json with network=host version
    fs::write(
        repo.path().join(".devcontainer/devcontainer.json"),
        formatdoc! {r#"
            {{
                "build": {{
                    "dockerfile": "Dockerfile",
                    "context": ".."
                }},
                "workspaceFolder": "{TEST_REPO_PATH}",
                "runArgs": ["--network=host"]
            }}
        "#},
    )
    .unwrap();
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "unsafe-host-test";

    // Enter pod and verify it works
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "hello"])
        .success()
        .expect("rumpel enter failed");

    // Verify git sync works by pushing a branch from pod
    pod_command(&repo, &daemon)
        .args([
            "enter", "--create", pod_name, "--", "git", "checkout", "-b", "feature",
        ])
        .success()
        .expect("git checkout failed");

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
            "feature commit",
        ])
        .success()
        .expect("git commit failed");

    // Get the commit hash from the pod
    let commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod commit");
    let pod_commit = String::from_utf8_lossy(&commit_output).trim().to_string();

    // Wait for the ref to land in the host
    let expected_ref = format!("refs/rumpelpod/feature@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &pod_commit);
}

// -- Git LFS tests ------------------------------------------------------------

/// Dockerfile snippet that installs git-lfs in the test container.
const LFS_DOCKERFILE: &str = "RUN apk add --no-cache git-lfs";

/// Set up an LFS-tracked file in a host repo and return the file's content.
///
/// Configures git-lfs, creates a `.gitattributes` tracking `*.bin` files,
/// writes a binary file, and commits everything.
fn setup_lfs_repo(repo_path: &Path) -> Vec<u8> {
    Command::new("git")
        .args(["lfs", "install", "--local"])
        .current_dir(repo_path)
        .success()
        .expect("git lfs install failed");

    Command::new("git")
        .args(["lfs", "track", "*.bin"])
        .current_dir(repo_path)
        .success()
        .expect("git lfs track failed");

    // Deterministic "large" content (just needs to be tracked by LFS)
    let content = b"lfs-test-content-1234567890\n";
    std::fs::write(repo_path.join("large.bin"), content).expect("write large.bin");

    Command::new("git")
        .args(["add", ".gitattributes", "large.bin"])
        .current_dir(repo_path)
        .success()
        .expect("git add failed");

    Command::new("git")
        .args(["commit", "-m", "add LFS file"])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .current_dir(repo_path)
        .success()
        .expect("git commit failed");

    content.to_vec()
}

/// Return the LFS object storage path for a given OID under a base dir.
fn lfs_object_path(base: &Path, oid: &str) -> PathBuf {
    base.join("lfs")
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
}

fn sha256_hex(content: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(content);
    hex::encode(h.finalize())
}

fn setup_lfs_repo_with_missing_historical_object(repo_path: &Path) -> String {
    Command::new("git")
        .args(["lfs", "install", "--local"])
        .current_dir(repo_path)
        .success()
        .expect("git lfs install failed");

    Command::new("git")
        .args(["config", "lfs.allowincompletepush", "false"])
        .current_dir(repo_path)
        .success()
        .expect("git config lfs.allowincompletepush failed");

    Command::new("git")
        .args(["lfs", "track", "*.bin"])
        .current_dir(repo_path)
        .success()
        .expect("git lfs track failed");

    let old_content = b"old lfs content no longer in head\n";
    std::fs::write(repo_path.join("old.bin"), old_content).expect("write old.bin");
    Command::new("git")
        .args(["add", ".gitattributes", "old.bin"])
        .current_dir(repo_path)
        .success()
        .expect("git add old.bin failed");
    create_commit(repo_path, "add old LFS file");

    Command::new("git")
        .args(["rm", "old.bin"])
        .current_dir(repo_path)
        .success()
        .expect("git rm old.bin failed");
    create_commit(repo_path, "remove old LFS file");

    let current_content = b"current lfs content in head\n";
    std::fs::write(repo_path.join("current.bin"), current_content).expect("write current.bin");
    Command::new("git")
        .args(["add", "current.bin"])
        .current_dir(repo_path)
        .success()
        .expect("git add current.bin failed");
    create_commit(repo_path, "add current LFS file");

    let old_oid = sha256_hex(old_content);
    let old_obj_path = lfs_object_path(&repo_path.join(".git"), &old_oid);
    fs::remove_file(&old_obj_path).unwrap_or_else(|e| {
        let old_obj_path = old_obj_path.display();
        panic!("remove old LFS object {old_obj_path}: {e}");
    });
    old_oid
}

#[test]
fn gateway_lfs_rejects_invalid_oid_paths() {
    let repo = TestRepo::new();
    let secret_path = repo.path().join("host-secret.txt");
    fs::write(&secret_path, "host secret outside lfs\n").expect("write host secret");

    assert!(
        secret_path.is_absolute(),
        "test path must exercise absolute traversal"
    );
    let encoded_secret_path = secret_path.to_string_lossy().replace('/', "%2F");
    let secret_path = secret_path.display().to_string();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "RUN apk add --no-cache -q curl >/dev/null 2>&1", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let pod_name = "lfs-invalid-oid";
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("pod enter failed");

    let download_script = formatdoc! {r#"
        set -eu
        url=$(git config --get remote.host.url)
        header=$(git config --get http.extraHeader)
        curl --path-as-is -sS -o /tmp/lfs-download-body -w '%{{http_code}}' \
            -H "$header" \
            "$url/lfs/objects/{encoded_secret_path}"
        printf '\n'
        cat /tmp/lfs-download-body
    "#};
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            &download_script,
        ])
        .success()
        .expect("download request failed");
    let stdout = String::from_utf8_lossy(&output);
    assert!(
        stdout.starts_with("400\n"),
        "encoded absolute download path should be rejected: {stdout}"
    );
    assert!(
        !stdout.contains("host secret outside lfs"),
        "download response should not include host file contents: {stdout}"
    );

    let batch_script = formatdoc! {r#"
        set -eu
        url=$(git config --get remote.host.url)
        header=$(git config --get http.extraHeader)
        curl -sS -o /tmp/lfs-batch-body -w '%{{http_code}}' \
            -H "$header" \
            -H 'Content-Type: application/vnd.git-lfs+json' \
            -X POST "$url/info/lfs/objects/batch" \
            --data '{{"operation":"download","objects":[{{"oid":"{secret_path}","size":1}}]}}'
        printf '\n'
        cat /tmp/lfs-batch-body
    "#};
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            &batch_script,
        ])
        .success()
        .expect("batch request failed");
    let stdout = String::from_utf8_lossy(&output);
    assert!(
        stdout.starts_with("400\n"),
        "batch request with an absolute OID should be rejected: {stdout}"
    );

    let upload_script = formatdoc! {r#"
        set -eu
        url=$(git config --get remote.host.url)
        header=$(git config --get http.extraHeader)
        curl --path-as-is -sS -o /tmp/lfs-upload-body -w '%{{http_code}}' \
            -H "$header" \
            -X PUT \
            --data-binary 'not an lfs object' \
            "$url/lfs/objects/{encoded_secret_path}"
        printf '\n'
        cat /tmp/lfs-upload-body
    "#};
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            &upload_script,
        ])
        .success()
        .expect("upload request failed");
    let stdout = String::from_utf8_lossy(&output);
    assert!(
        stdout.starts_with("400\n"),
        "upload request with an absolute OID should be rejected: {stdout}"
    );
}

#[test]
fn gateway_lfs_missing_historical_object_does_not_block_pod_push() {
    let repo = TestRepo::new();
    let missing_oid = setup_lfs_repo_with_missing_historical_object(repo.path());

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, LFS_DOCKERFILE, "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "lfs-missing-history";

    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "cat", "current.bin"])
        .success()
        .expect("pod enter failed");

    let script = "printf 'pod note\n' > note.txt && git add note.txt && git commit --no-verify -m 'add non-lfs note'";
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "sh", "-c", script])
        .success()
        .expect("pod commit failed");

    let pod_commit_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod commit");
    let pod_commit = String::from_utf8_lossy(&pod_commit_output)
        .trim()
        .to_string();

    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    assert!(
        wait_for_ref_commit_until(
            repo.path(),
            &expected_ref,
            &pod_commit,
            std::time::Duration::from_secs(20),
        ),
        "pod push should not require missing historical LFS object {missing_oid}"
    );
}

#[test]
fn gateway_lfs_download_from_host() {
    // Verify that an LFS file committed on the host is available in the
    // pod with full content (not a pointer).
    let repo = TestRepo::new();
    let content = setup_lfs_repo(repo.path());

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, LFS_DOCKERFILE, "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "lfs-download";

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "cat", "large.bin"])
        .success()
        .expect("pod enter failed");

    assert_eq!(
        output, content,
        "LFS file content in pod should match host content"
    );
}

#[test]
fn gateway_lfs_upload_from_pod() {
    // Verify that creating an LFS file inside a pod stores the object
    // in the host's .git/lfs directory.
    let repo = TestRepo::new();

    // Set up LFS tracking in the host repo so .gitattributes is present
    Command::new("git")
        .args(["lfs", "install", "--local"])
        .current_dir(repo.path())
        .success()
        .expect("git lfs install failed");

    Command::new("git")
        .args(["lfs", "track", "*.bin"])
        .current_dir(repo.path())
        .success()
        .expect("git lfs track failed");

    Command::new("git")
        .args(["add", ".gitattributes"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");

    Command::new("git")
        .args(["commit", "-m", "track bin files with LFS"])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .current_dir(repo.path())
        .success()
        .expect("git commit failed");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, LFS_DOCKERFILE, "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "lfs-upload";

    // Create an LFS file inside the pod, commit, and push
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            "echo pod-lfs-content > upload.bin && git add upload.bin && git commit --no-verify -m 'add upload.bin'",
        ])
        .success()
        .expect("pod commit failed");

    // The pod's hook should have pushed to the host.  The LFS
    // pre-push hook uploads the object to the host's LFS storage
    // via our HTTP server.

    // Compute the expected OID (sha256 of the file content)
    let file_content = b"pod-lfs-content\n";
    let oid = {
        let mut h = Sha256::new();
        h.update(file_content);
        hex::encode(h.finalize())
    };

    // LFS objects land directly in the host repo's .git directory.
    let obj_path = lfs_object_path(&repo.path().join(".git"), &oid);

    assert!(
        obj_path.exists(),
        "LFS object should exist in gateway storage at {}",
        obj_path.display()
    );

    let stored = std::fs::read(&obj_path).expect("read LFS object");
    assert_eq!(
        stored, file_content,
        "Stored LFS object content should match"
    );
}

#[test]
fn gateway_lfs_not_used() {
    // Pod creation must succeed when the container has git-lfs installed
    // but the repo has no LFS-tracked files.
    let repo = TestRepo::new();
    std::fs::write(repo.path().join("plain.txt"), "hello\n").expect("write file");
    Command::new("git")
        .args(["add", "plain.txt"])
        .current_dir(repo.path())
        .success()
        .expect("git add failed");
    create_commit(repo.path(), "add plain file");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, LFS_DOCKERFILE, "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "no-lfs", "--", "cat", "plain.txt"])
        .success()
        .expect("pod enter failed for non-LFS repo with git-lfs installed");

    assert_eq!(output, b"hello\n", "file content should match");
}

#[test]
fn pod_has_host_remotes() {
    // Remotes from the host repo (other than rumpelpod-managed ones)
    // should be present inside the pod.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Add an "origin" remote to the host repo before launching.
    Command::new("git")
        .args(["remote", "add", "origin", "https://example.com/repo.git"])
        .current_dir(repo.path())
        .success()
        .expect("adding origin remote failed");

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "remote-sync", "--", "echo", "setup"])
        .success()
        .expect("rumpel enter failed");

    // The pod should have the "origin" remote with the host's URL.
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "remote-sync",
            "--",
            "git",
            "remote",
            "get-url",
            "origin",
        ])
        .success()
        .expect("getting origin URL in pod failed");
    let pod_origin_url = String::from_utf8_lossy(&output).trim().to_string();
    assert_eq!(pod_origin_url, "https://example.com/repo.git");
}

#[test]
fn pods_see_each_others_branches() {
    // When two pods (foo, bar) are running, each pod should see the other's
    // primary branch as rumpelpod/<other_pod> after git fetch rumpelpod.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Launch two pods
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "foo", "--", "echo", "setup"])
        .success()
        .expect("Failed to launch pod foo");

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "bar", "--", "echo", "setup"])
        .success()
        .expect("Failed to launch pod bar");

    // Create a commit in bar so its primary branch is pushed
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "bar",
            "--",
            "git",
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "commit from bar",
        ])
        .success()
        .expect("Failed to create commit in bar");

    // Get bar's HEAD for polling
    let bar_head = pod_command(&repo, &daemon)
        .args(["enter", "--create", "bar", "--", "git", "rev-parse", "HEAD"])
        .success()
        .expect("getting bar HEAD failed");
    let bar_commit = String::from_utf8_lossy(&bar_head).trim().to_string();

    // Wait for bar's ref to land in host so it is fetchable
    wait_for_ref_commit(repo.path(), "refs/rumpelpod/bar@bar", &bar_commit);

    // From foo: fetch the rumpelpod remote, then list remote-tracking refs
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "foo",
            "--",
            "git",
            "fetch",
            "rumpelpod",
        ])
        .success()
        .expect("git fetch rumpelpod failed in foo");

    let refs_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "foo",
            "--",
            "git",
            "for-each-ref",
            "--format=%(refname)",
            "refs/remotes/rumpelpod/",
        ])
        .success()
        .expect("listing rumpelpod refs in foo failed");
    let refs = String::from_utf8_lossy(&refs_output);

    // Should have rumpelpod/bar (the alias), not rumpelpod/rumpelpod/bar
    assert!(
        refs.contains("refs/remotes/rumpelpod/bar"),
        "foo should see rumpelpod/bar, got:\n{refs}",
    );
    assert!(
        !refs.contains("refs/remotes/rumpelpod/rumpelpod/"),
        "foo should not have double-prefixed refs, got:\n{refs}",
    );

    // The alias should resolve to the same commit as bar's HEAD
    let foo_ref = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "foo",
            "--",
            "git",
            "rev-parse",
            "rumpelpod/bar",
        ])
        .success()
        .expect("resolving rumpelpod/bar in foo failed");
    let foo_sees_commit = String::from_utf8_lossy(&foo_ref).trim().to_string();

    assert_eq!(
        foo_sees_commit, bar_commit,
        "rumpelpod/bar in foo should point to bar's HEAD",
    );
}

#[test]
fn gateway_reconnect_push() {
    // The reconnect steps poke the running container directly with
    // `docker exec`, so only the localhost Docker executor applies.
    // k8s has its own k8s_gateway_reconnect_push covering the same
    // behaviour through kubectl exec.
    if !matches!(executor::executor_mode(), executor::ExecutorMode::Docker) {
        executor::skip_test();
        return;
    }
    // Commits made while the daemon is down should be pushed to the
    // host when the daemon reconnects.  The pod's /events endpoint
    // triggers a push of all local branches on each new connection.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let mut daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "reconnect-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("initial enter failed");

    let host_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .success()
        .expect("rev-parse HEAD on host failed");
    let host_head = String::from_utf8_lossy(&host_head).trim().to_string();
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &host_head);

    // Find the container ID by the pod-name label rumpelpod sets at
    // creation time.  Robust against container-naming changes.
    let container_id = {
        let output = Command::new("docker")
            .args([
                "ps",
                "-q",
                "--filter",
                &format!("label=dev.rumpelpod.name={pod_name}"),
            ])
            .success()
            .expect("docker ps failed");
        String::from_utf8_lossy(&output)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    };
    assert!(
        !container_id.is_empty(),
        "container not found for pod {pod_name}"
    );

    // Stop the daemon so the tunnel goes down.  The tunnel server
    // inside the container exits on stdin close, so the listening port
    // is freed and git push gets connection refused.
    daemon.kill();

    // Create a commit inside the container while the daemon is down.
    // The tunnel-server exits on stdin close so the hook push gets
    // connection refused.
    Command::new("docker")
        .args([
            "exec",
            &container_id,
            "env",
            "GIT_HTTP_LOW_SPEED_LIMIT=1",
            "GIT_HTTP_LOW_SPEED_TIME=10",
            "git",
            "-C",
            TEST_REPO_PATH,
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "offline commit",
        ])
        .success()
        .expect("docker exec git commit failed");

    let rev_output = Command::new("docker")
        .args([
            "exec",
            &container_id,
            "git",
            "-C",
            TEST_REPO_PATH,
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("docker exec git rev-parse failed");
    let offline_commit = String::from_utf8_lossy(&rev_output).trim().to_string();

    // The host should NOT have this commit yet (daemon was down).
    let host_commit_before = get_pod_ref_commit(repo.path(), &expected_ref);
    assert_ne!(
        host_commit_before.as_deref(),
        Some(offline_commit.as_str()),
        "host should not have the offline commit yet"
    );

    // Restart the daemon and re-enter the pod.  The daemon connects to
    // /events, which causes the pod to push all branches.
    let daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "true"])
        .success()
        .expect("re-enter after restart failed");

    // The reconnect push is asynchronous, so poll until it lands.
    wait_for_ref_commit(repo.path(), &expected_ref, &offline_commit);
}

#[test]
fn gateway_daemon_restart_pushes_without_reenter() {
    // Restore-on-startup only works for localhost Docker pods.
    if !matches!(executor::executor_mode(), executor::ExecutorMode::Docker) {
        executor::skip_test();
        return;
    }
    // Commits made while the daemon is down should be pushed when the
    // daemon restarts, without anyone re-entering the pod.  The daemon
    // restores connections to running pods on startup.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let mut daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "restart-push-test";

    // Launch pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("initial enter failed");

    let host_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .success()
        .expect("rev-parse HEAD on host failed");
    let host_head = String::from_utf8_lossy(&host_head).trim().to_string();
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    wait_for_ref_commit(repo.path(), &expected_ref, &host_head);

    // Find the container ID by the pod-name label (see
    // gateway_reconnect_push for why we prefer this over name).
    let container_id = {
        let output = Command::new("docker")
            .args([
                "ps",
                "-q",
                "--filter",
                &format!("label=dev.rumpelpod.name={pod_name}"),
            ])
            .success()
            .expect("docker ps failed");
        String::from_utf8_lossy(&output)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    };
    assert!(
        !container_id.is_empty(),
        "container not found for pod {pod_name}"
    );

    // Kill the daemon so the tunnel goes down.
    daemon.kill();

    // Create a commit inside the container while the daemon is down.
    Command::new("docker")
        .args([
            "exec",
            &container_id,
            "env",
            "GIT_HTTP_LOW_SPEED_LIMIT=1",
            "GIT_HTTP_LOW_SPEED_TIME=10",
            "git",
            "-C",
            TEST_REPO_PATH,
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "offline commit",
        ])
        .success()
        .expect("docker exec git commit failed");

    let rev_output = Command::new("docker")
        .args([
            "exec",
            &container_id,
            "git",
            "-C",
            TEST_REPO_PATH,
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("docker exec git rev-parse failed");
    let offline_commit = String::from_utf8_lossy(&rev_output).trim().to_string();

    // The host should NOT have this commit yet.
    let host_commit_before = get_pod_ref_commit(repo.path(), &expected_ref);
    assert_ne!(
        host_commit_before.as_deref(),
        Some(offline_commit.as_str()),
        "host should not have the offline commit yet"
    );

    // Restart the daemon -- do NOT re-enter the pod.  The daemon
    // should restore connections to running pods on startup and the
    // /events handler pushes all branches on connect.
    let _daemon = TestDaemon::start(&home);

    wait_for_ref_commit(repo.path(), &expected_ref, &offline_commit);
}
