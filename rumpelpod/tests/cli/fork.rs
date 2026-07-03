// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `rumpel fork`.

use std::fs;

use crate::common::{
    ensure_pi_config_via_daemon, launch_pod_via_daemon, pod_command, write_host_pi_settings,
    write_test_devcontainer, write_test_devcontainer_with_fake_pi, TestDaemon, TestHome, TestRepo,
};
use crate::executor::ExecutorResources;
use rumpelpod::CommandExt;

#[test]
fn fork_smoke_test() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // 1. Create the source pod and a marker file inside the repo.
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "src",
            "--",
            "sh",
            "-c",
            "echo source > marker.txt",
        ])
        .success()
        .expect("rumpel enter src failed");

    // 2. Fork it.
    pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .success()
        .expect("rumpel fork failed");

    // 3. The new pod must exist with the marker carried across.
    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "fk", "--", "cat", "marker.txt"])
        .success()
        .expect("read marker on fork failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "source");

    // 4. Primary branch is renamed: rumpelpod.pod-name should be "fk".
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "fk",
            "--",
            "git",
            "config",
            "--get",
            "rumpelpod.pod-name",
        ])
        .success()
        .expect("read rumpelpod.pod-name on fork failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "fk");
}

#[test]
fn fork_preserves_dirty_tree() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // 1. Source pod with a dirty (uncommitted) file.
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "src",
            "--",
            "sh",
            "-c",
            "echo dirty > scratch.txt",
        ])
        .success()
        .expect("rumpel enter src failed");

    // 2. Fork.
    pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .success()
        .expect("rumpel fork failed");

    // 3. The dirty file must be present on the fork too.
    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "fk", "--", "cat", "scratch.txt"])
        .success()
        .expect("read scratch on fork failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "dirty");
}

#[test]
fn fork_preserves_codex_state_symlinks() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "src",
            "--",
            "sh",
            "-c",
            r#"mkdir -p "$HOME/.codex/tmp" && printf target > /tmp/codex-target && ln -s /tmp/codex-target "$HOME/.codex/tmp/link""#,
        ])
        .success()
        .expect("create codex symlink state failed");

    pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .success()
        .expect("rumpel fork failed");

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "fk",
            "--",
            "sh",
            "-c",
            r#"test -L "$HOME/.codex/tmp/link" && readlink "$HOME/.codex/tmp/link""#,
        ])
        .success()
        .expect("read forked codex symlink failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "/tmp/codex-target");
}

#[test]
fn fork_preserves_pi_state() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer_with_fake_pi(&repo);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let source_launch = launch_pod_via_daemon(&repo, &daemon, "src");
    write_host_pi_settings(&home, "host-before");
    ensure_pi_config_via_daemon(&repo, &daemon, "src", &source_launch);

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "src",
            "--",
            "sh",
            "-c",
            r#"mkdir -p "$HOME/.pi/agent" && printf '{"defaultProjectTrust":"always","source":"pod"}' > "$HOME/.pi/agent/settings.json""#,
        ])
        .success()
        .expect("create pi state failed");

    write_host_pi_settings(&home, "host-after");

    pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .success()
        .expect("rumpel fork failed");

    let fork_launch = launch_pod_via_daemon(&repo, &daemon, "fk");
    ensure_pi_config_via_daemon(&repo, &daemon, "fk", &fork_launch);

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "fk",
            "--",
            "cat",
            "/home/testuser/.pi/agent/settings.json",
        ])
        .success()
        .expect("read forked pi state failed");
    let stdout = String::from_utf8_lossy(&stdout);
    assert!(stdout.contains(r#""source":"pod""#), "{stdout}");
}

#[test]
fn fork_preserves_secondary_branch() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // 1. Source pod with a secondary branch tracking host/master.
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "src", "--", "true"])
        .success()
        .expect("rumpel enter src failed");
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "src",
            "--",
            "git",
            "branch",
            "--track",
            "feature",
            "host/master",
        ])
        .success()
        .expect("create feature branch failed");

    // 2. Fork.
    pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .success()
        .expect("rumpel fork failed");

    // 3. The secondary branch should exist on the fork with its host/* upstream preserved.
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "fk",
            "--",
            "git",
            "for-each-ref",
            "refs/heads/feature",
            "--format=%(upstream:short)",
        ])
        .success()
        .expect("read feature upstream on fork failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "host/master");
}

/// --env-file entries in runArgs are resolved into containerEnv at
/// source-launch time but never persisted, so a naive fork loses them.
/// Fork snapshots the source pod's resolved env via /container-env, so
/// the vars survive.
#[test]
fn fork_preserves_env_file() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(
        repo.path().join(".env"),
        "FORK_SECRET=hunter2\nOTHER=world\n",
    )
    .unwrap();
    write_test_devcontainer(&repo, "", r#", "runArgs": ["--env-file", ".env"]"#);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "src",
            "--",
            "sh",
            "-c",
            "echo $FORK_SECRET $OTHER",
        ])
        .success()
        .expect("rumpel enter src failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hunter2 world");

    pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .success()
        .expect("rumpel fork failed");

    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "fk", "--", "sh", "-c", "echo $FORK_SECRET $OTHER"])
        .success()
        .expect("rumpel enter fk failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hunter2 world");
}

/// The fork must use the env values the source container actually saw,
/// not whatever the daemon would resolve from the current local repo.
/// Editing the env-file after source creation must not leak into the
/// fork -- otherwise the snapshot is just disk re-resolution wearing
/// a costume.
#[test]
fn fork_uses_source_container_env_not_current_disk() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".env"), "FORK_VAR=original\n").unwrap();
    write_test_devcontainer(&repo, "", r#", "runArgs": ["--env-file", ".env"]"#);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "src",
            "--",
            "sh",
            "-c",
            "echo $FORK_VAR",
        ])
        .success()
        .expect("rumpel enter src failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "original");

    // Mutate the env-file after the source pod is up.  A from-disk
    // re-resolution at fork time would pick this up; a true container
    // snapshot must not.
    fs::write(repo.path().join(".env"), "FORK_VAR=changed\n").unwrap();

    pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .success()
        .expect("rumpel fork failed");

    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "fk", "--", "sh", "-c", "echo $FORK_VAR"])
        .success()
        .expect("rumpel enter fk failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "original");
}

#[test]
fn fork_nonexistent_source_fails() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["fork", "no-such-pod", "fk"])
        .output()
        .expect("rumpel fork did not run");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist"),
        "expected 'does not exist' in stderr, got: {stderr}"
    );
}

#[test]
fn fork_collision_fails() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "src", "--", "true"])
        .success()
        .expect("create src failed");
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "fk", "--", "true"])
        .success()
        .expect("create fk failed");

    let output = pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .output()
        .expect("rumpel fork did not run");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' in stderr, got: {stderr}"
    );
}
