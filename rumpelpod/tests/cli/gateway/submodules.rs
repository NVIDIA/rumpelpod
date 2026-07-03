// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::process::Command;

use indoc::indoc;
use rumpelpod::CommandExt;

use super::get_pod_ref_commit;
use crate::common::{
    create_commit, pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo,
};
use crate::executor::{self, ExecutorResources};

/// Create a parent TestRepo that contains a git submodule pointing to
/// a second TestRepo.  Returns (parent, child_name) where child_name
/// is the submodule name/path.
fn create_test_repo_with_submodule() -> (TestRepo, TestRepo, String) {
    let child = TestRepo::new();
    create_commit(child.path(), "Child initial");

    let parent = TestRepo::new();

    let child_name = "child-sub";

    // Add the child repo as a submodule of the parent.
    // -c protocol.file.allow=always: needed because newer git versions
    // block file:// transport by default for security.
    Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &child.path().to_string_lossy(),
            child_name,
        ])
        .current_dir(parent.path())
        .success()
        .expect("git submodule add failed");

    Command::new("git")
        .args(["commit", "-m", "Add submodule"])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .current_dir(parent.path())
        .success()
        .expect("git commit (submodule add) failed");

    (parent, child, child_name.to_string())
}

#[test]
fn submodule_pod_commit_syncs_to_host() {
    let (parent, _child, sub_name) = create_test_repo_with_submodule();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&parent, "", "");
    fs::write(parent.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "sub-commit-test";

    // Launch pod
    pod_command(&parent, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit inside the pod's submodule
    let commit_script = format!(
        "cd {sub_name} && \
         git config user.email test@example.com && \
         git config user.name TestUser && \
         git commit --allow-empty -m 'Pod submodule commit'"
    );
    pod_command(&parent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            &commit_script,
        ])
        .success()
        .expect("Failed to create commit in pod submodule");

    // Get the commit hash from the pod's submodule
    let pod_sub_commit = pod_command(&parent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "-C",
            &sub_name,
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod submodule commit");
    let pod_sub_commit = String::from_utf8_lossy(&pod_sub_commit).trim().to_string();

    // Pod refs land directly in the submodule's git directory.
    let sub_workdir = parent.path().join(&sub_name);
    let sub_git_dir =
        rumpelpod::gateway::resolve_git_dir(&sub_workdir).expect("resolving submodule git dir");

    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    let sub_commit = get_pod_ref_commit(&sub_git_dir, &expected_ref);
    assert_eq!(
        sub_commit,
        Some(pod_sub_commit),
        "Submodule should have ref '{expected_ref}' with pod's commit"
    );
}

#[test]
fn submodule_host_update_visible_in_pod() {
    let (parent, _child, sub_name) = create_test_repo_with_submodule();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&parent, "", "");
    fs::write(parent.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "sub-fetch-test";

    // Launch pod
    pod_command(&parent, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit in the host's submodule.
    // Submodule worktrees do not inherit the parent repo's local git
    // config, so set identity via env vars for portability.
    let host_sub_path = parent.path().join(&sub_name);
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "Host submodule update"])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(&host_sub_path)
        .success()
        .expect("Failed to commit in host submodule");

    let host_sub_commit: String = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&host_sub_path)
        .success()
        .expect("Failed to get host submodule commit")
        .try_into()
        .unwrap();
    let host_sub_commit = host_sub_commit.trim().to_string();

    // Fetch host refs inside the pod's submodule
    pod_command(&parent, &daemon)
        .args([
            "enter", pod_name, "--", "git", "-C", &sub_name, "fetch", "host",
        ])
        .success()
        .expect("Failed to fetch host in pod submodule");

    // The pod should see the new commit via host/HEAD
    let fetched_commit = pod_command(&parent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "-C",
            &sub_name,
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to get host/HEAD in pod submodule");
    let fetched_commit = String::from_utf8_lossy(&fetched_commit).trim().to_string();

    assert_eq!(
        fetched_commit, host_sub_commit,
        "Pod should see the host submodule commit via host/HEAD"
    );
}

#[test]
fn submodule_pod_commit_mirrored_to_host() {
    let (parent, _child, sub_name) = create_test_repo_with_submodule();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&parent, "", "");
    fs::write(parent.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "sub-mirror-test";

    // Launch pod
    pod_command(&parent, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit in the pod's submodule
    let commit_script = format!(
        "cd {sub_name} && \
         git config user.email test@example.com && \
         git config user.name TestUser && \
         git commit --allow-empty -m 'Mirror test commit'"
    );
    pod_command(&parent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            &commit_script,
        ])
        .success()
        .expect("Failed to create commit in pod submodule");

    let pod_sub_commit = pod_command(&parent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "-C",
            &sub_name,
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod submodule commit");
    let pod_sub_commit = String::from_utf8_lossy(&pod_sub_commit).trim().to_string();

    // The host submodule should have the pod's commit at refs/rumpelpod/.
    let host_sub_path = parent.path().join(&sub_name);
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");

    let pod_ref_commit = get_pod_ref_commit(&host_sub_path, &expected_ref);
    assert_eq!(
        pod_ref_commit,
        Some(pod_sub_commit),
        "Host submodule should have ref '{expected_ref}' with pod's commit"
    );
}

/// Create a parent TestRepo that contains a git submodule whose path lives
/// inside a subdirectory (e.g. "libs/child-sub").  The subdirectory itself is
/// NOT a submodule -- just a regular directory that happens to be part of the
/// submodule path.  This exercises the case where `displaypath` (and `name`)
/// contain a slash without actually being a nested submodule.
/// Returns (parent, child, submodule_name).
fn create_test_repo_with_subdir_submodule() -> (TestRepo, TestRepo, String) {
    let child = TestRepo::new();
    create_commit(child.path(), "Child initial");

    let parent = TestRepo::new();

    let sub_name = "libs/child-sub";

    Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &child.path().to_string_lossy(),
            sub_name,
        ])
        .current_dir(parent.path())
        .success()
        .expect("git submodule add failed");

    Command::new("git")
        .args(["commit", "-m", "Add submodule in subdirectory"])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .current_dir(parent.path())
        .success()
        .expect("git commit (submodule add) failed");

    (parent, child, sub_name.to_string())
}

#[test]
fn subdir_submodule_pod_commit_syncs_to_host() {
    let (parent, _child, sub_name) = create_test_repo_with_subdir_submodule();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&parent, "", "");
    fs::write(parent.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "subdir-commit-test";

    // Launch pod
    pod_command(&parent, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit inside the pod's submodule
    let commit_script = format!(
        "cd {sub_name} && \
         git config user.email test@example.com && \
         git config user.name TestUser && \
         git commit --allow-empty -m 'Pod subdir submodule commit'"
    );
    pod_command(&parent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            &commit_script,
        ])
        .success()
        .expect("Failed to create commit in pod subdir submodule");

    // Get the commit hash from the pod's submodule
    let pod_sub_commit = pod_command(&parent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "-C",
            &sub_name,
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod subdir submodule commit");
    let pod_sub_commit = String::from_utf8_lossy(&pod_sub_commit).trim().to_string();

    // Pod refs land directly in the submodule's git directory.
    let sub_workdir = parent.path().join("libs").join("child-sub");
    let sub_git_dir =
        rumpelpod::gateway::resolve_git_dir(&sub_workdir).expect("resolving submodule git dir");

    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    let sub_commit = get_pod_ref_commit(&sub_git_dir, &expected_ref);
    assert_eq!(
        sub_commit,
        Some(pod_sub_commit),
        "Submodule should have ref '{expected_ref}' with pod's commit"
    );
}

/// Create a three-level hierarchy: grandparent -> outer-sub -> inner-sub.
/// Returns (grandparent, outer_child, inner_child, outer_name, inner_displaypath).
fn create_test_repo_with_nested_submodules() -> (TestRepo, TestRepo, TestRepo, String, String) {
    let inner = TestRepo::new();
    create_commit(inner.path(), "Inner initial");

    let outer = TestRepo::new();

    let inner_name = "inner-sub";
    Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &inner.path().to_string_lossy(),
            inner_name,
        ])
        .current_dir(outer.path())
        .success()
        .expect("git submodule add (inner) failed");
    Command::new("git")
        .args(["commit", "-m", "Add inner submodule"])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .current_dir(outer.path())
        .success()
        .expect("git commit (inner submodule) failed");

    let grandparent = TestRepo::new();

    let outer_name = "outer-sub";
    Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &outer.path().to_string_lossy(),
            outer_name,
        ])
        .current_dir(grandparent.path())
        .success()
        .expect("git submodule add (outer) failed");
    Command::new("git")
        .args(["commit", "-m", "Add outer submodule"])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .current_dir(grandparent.path())
        .success()
        .expect("git commit (outer submodule) failed");

    let inner_displaypath = format!("{}/{}", outer_name, inner_name);
    (
        grandparent,
        outer,
        inner,
        outer_name.to_string(),
        inner_displaypath,
    )
}

#[test]
fn nested_submodule_pod_commit_syncs_to_host() {
    let (grandparent, _outer, _inner, _outer_name, inner_displaypath) =
        create_test_repo_with_nested_submodules();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&grandparent, "", "");
    fs::write(grandparent.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "nested-commit-test";

    // Launch pod
    pod_command(&grandparent, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit inside the pod's nested (inner) submodule
    let commit_script = format!(
        "cd {inner_displaypath} && \
         git config user.email test@example.com && \
         git config user.name TestUser && \
         git commit --allow-empty -m 'Pod nested submodule commit'"
    );
    pod_command(&grandparent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "sh",
            "-c",
            &commit_script,
        ])
        .success()
        .expect("Failed to create commit in pod nested submodule");

    // Get the commit hash from the pod's nested submodule
    let pod_commit = pod_command(&grandparent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "-C",
            &inner_displaypath,
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("Failed to get pod nested submodule commit");
    let pod_commit = String::from_utf8_lossy(&pod_commit).trim().to_string();

    // Pod refs land directly in the nested submodule's git directory.
    let inner_workdir = grandparent.path().join(&inner_displaypath);
    let inner_git_dir = rumpelpod::gateway::resolve_git_dir(&inner_workdir)
        .expect("resolving nested submodule git dir");

    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");
    let inner_commit = get_pod_ref_commit(&inner_git_dir, &expected_ref);
    assert_eq!(
        inner_commit,
        Some(pod_commit),
        "Nested submodule should have ref '{expected_ref}' with pod's commit"
    );
}

#[test]
fn nested_submodule_host_update_visible_in_pod() {
    let (grandparent, _outer, _inner, _outer_name, inner_displaypath) =
        create_test_repo_with_nested_submodules();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&grandparent, "", "");
    fs::write(grandparent.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "nested-fetch-test";

    // Launch pod
    pod_command(&grandparent, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("Failed to run rumpel enter");

    // Create a commit in the host's nested submodule
    let host_inner_path = grandparent.path().join(&inner_displaypath);
    Command::new("git")
        .args([
            "commit",
            "--allow-empty",
            "-m",
            "Host nested submodule update",
        ])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(&host_inner_path)
        .success()
        .expect("Failed to commit in host nested submodule");

    let host_inner_commit: String = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&host_inner_path)
        .success()
        .expect("Failed to get host nested submodule commit")
        .try_into()
        .unwrap();
    let host_inner_commit = host_inner_commit.trim().to_string();

    // Fetch host refs inside the pod's nested submodule
    pod_command(&grandparent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "-C",
            &inner_displaypath,
            "fetch",
            "host",
        ])
        .success()
        .expect("Failed to fetch host in pod nested submodule");

    // The pod should see the new commit via host/HEAD
    let fetched_commit = pod_command(&grandparent, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "-C",
            &inner_displaypath,
            "rev-parse",
            "host/HEAD",
        ])
        .success()
        .expect("Failed to get host/HEAD in pod nested submodule");
    let fetched_commit = String::from_utf8_lossy(&fetched_commit).trim().to_string();

    assert_eq!(
        fetched_commit, host_inner_commit,
        "Pod should see the host nested submodule commit via host/HEAD"
    );
}

#[test]
fn uninitialized_submodule_keeps_parent_remotes_after_restart() {
    // Restarting the daemon replaces the tunnel, which triggers the
    // gateway URL refresh in the pod.  Only the Docker exec tunnel
    // dies together with the daemon.
    if !matches!(executor::executor_mode(), executor::ExecutorMode::Docker) {
        executor::skip_test();
        return;
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let mut daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let pod_name = "uninit-sub-refresh";

    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "setup"])
        .success()
        .expect("initial enter failed");

    // A .gitmodules entry whose submodule was never initialized, as
    // after switching to a branch that adds a submodule: the worktree
    // only contains an empty placeholder directory.  Git commands run
    // in the placeholder resolve to the parent repo, so the gateway
    // refresh must skip the entry or it corrupts the parent remotes.
    let add_entry = indoc! {r#"
        set -eu
        printf '[submodule "vendor"]\n\tpath = vendor\n\turl = https://example.invalid/vendor.git\n' > .gitmodules
        mkdir vendor
    "#};
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "sh", "-c", add_entry])
        .success()
        .expect("adding uninitialized submodule entry failed");

    daemon.kill();
    let daemon = TestDaemon::start(&home);

    let url = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "git",
            "config",
            "--get",
            "remote.rumpelpod.url",
        ])
        .success()
        .expect("reading rumpelpod remote failed");
    let url = String::from_utf8_lossy(&url).trim().to_string();
    assert!(
        !url.contains("/submodules/"),
        "parent rumpelpod remote was rewritten to submodule URL: {url}"
    );

    let push_script = indoc! {r#"
        set -eu
        git commit --no-verify --allow-empty -m "post restart push"
        GIT_HTTP_LOW_SPEED_LIMIT=1 \
        GIT_HTTP_LOW_SPEED_TIME=3 \
        git push rumpelpod --force --quiet
    "#};
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "sh", "-c", push_script])
        .success()
        .expect("git push rumpelpod should work after daemon restart");
}
