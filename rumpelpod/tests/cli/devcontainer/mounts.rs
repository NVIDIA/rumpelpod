// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for devcontainer.json mounts configuration:
//! volume, tmpfs, and string-format mounts.

use indoc::formatdoc;
use rumpelpod::CommandExt;
use std::fs;

use crate::common::{pod_command, TestDaemon, TestHome, TestRepo, TEST_REPO_PATH, TEST_USER};
use crate::executor::ExecutorResources;
use crate::ssh::{write_ssh_config, SshRemoteHost};

/// Extract a short unique ID from a TestRepo's temp directory name.
/// The TempDir suffix is random, so this is safe for concurrent use.
fn repo_id(repo: &TestRepo) -> String {
    repo.path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string()
}

fn write_devcontainer_with_mounts(repo: &TestRepo, mounts_config: &str) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("Failed to create .devcontainer directory");

    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git shadow
        RUN useradd -m -u 1000 {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
    "#};
    fs::write(devcontainer_dir.join("Dockerfile"), dockerfile).expect("Failed to write Dockerfile");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": ".."
            }},
            "mounts": {mounts_config},
            "workspaceFolder": "{TEST_REPO_PATH}",
            "containerUser": "{TEST_USER}"
        }}
    "#};

    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("Failed to write devcontainer.json");
}

/// Volume mount: the target directory should exist and be writable inside the container.
///
/// Docker named volumes are created implicitly when first referenced in a
/// container mount.  We derive the volume name from the repo's temp-dir so
/// that concurrent test runs never collide.
#[test]
fn mount_volume() {
    let repo = TestRepo::new();
    let vol = format!("vol-{}", repo_id(&repo));

    write_devcontainer_with_mounts(
        &repo,
        &format!(r#"[{{"type": "volume", "source": "{vol}", "target": "/data"}}]"#),
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Write a file to /data and read it back to confirm the mount is writable
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mnt-vol",
            "--",
            "sh",
            "-c",
            "echo hello > /data/testfile && cat /data/testfile",
        ])
        .success()
        .expect("rumpel enter failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(stdout.trim(), "hello");
}

/// tmpfs mount: the target should appear as a tmpfs filesystem.
#[test]
fn mount_tmpfs() {
    let repo = TestRepo::new();

    write_devcontainer_with_mounts(&repo, r#"[{"type": "tmpfs", "target": "/tmp/mytmp"}]"#);
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Verify the mount point exists and is a tmpfs via /proc/mounts
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mnt-tmpfs",
            "--",
            "sh",
            "-c",
            "grep /tmp/mytmp /proc/mounts",
        ])
        .success()
        .expect("rumpel enter failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        stdout.contains("tmpfs"),
        "expected tmpfs mount at /tmp/mytmp, got: {stdout}"
    );
}

/// String-format mount: Docker --mount style comma-separated key=value pairs
/// should be parsed the same as the object format.
#[test]
fn mount_string_format() {
    let repo = TestRepo::new();
    let vol = format!("vol-{}", repo_id(&repo));

    write_devcontainer_with_mounts(
        &repo,
        &format!(r#"["type=volume,source={vol},target=/mnt"]"#),
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Write and read back to confirm the volume is mounted and writable
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mnt-strfmt",
            "--",
            "sh",
            "-c",
            "echo ok > /mnt/check && cat /mnt/check",
        ])
        .success()
        .expect("rumpel enter failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(stdout.trim(), "ok");
}

/// Volume data should survive a container restart because Docker volumes
/// persist independently of the container lifecycle.
#[test]
fn mount_persists_across_restarts() {
    let repo = TestRepo::new();
    let vol = format!("vol-{}", repo_id(&repo));

    write_devcontainer_with_mounts(
        &repo,
        &format!(r#"[{{"type": "volume", "source": "{vol}", "target": "/data"}}]"#),
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // First run: write a sentinel file into the volume
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mnt-persist",
            "--",
            "sh",
            "-c",
            "echo persisted > /data/sentinel",
        ])
        .success()
        .expect("first rumpel enter failed");

    // Recreate the container (volume should survive)
    pod_command(&repo, &daemon)
        .args(["recreate", "mnt-persist"])
        .success()
        .expect("pod recreate failed");

    // Second run: the sentinel should still be there
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mnt-persist",
            "--",
            "cat",
            "/data/sentinel",
        ])
        .success()
        .expect("second rumpel enter failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(stdout.trim(), "persisted");
}

/// Bind mounts on remote Docker hosts are converted to volumes and
/// populated via tar upload.  The source directory contents should
/// appear at the target path inside the container.
#[test]
fn mount_bind_remote_copies_content() {
    // Spins up its own SSH-over-Docker remote, which requires the
    // Docker executor.  The k8s mount path has its own coverage.
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker
    ) {
        crate::executor::skip_test();
        return;
    }
    let repo = TestRepo::new();

    // Create a source directory with test content on the host.
    let bind_src = repo.path().join("bind-src");
    fs::create_dir_all(bind_src.join("sub")).unwrap();
    fs::write(bind_src.join("hello.txt"), "from-host\n").unwrap();
    fs::write(bind_src.join("sub/nested.txt"), "nested\n").unwrap();

    let bind_src_str = bind_src.to_string_lossy();
    write_devcontainer_with_mounts(
        &repo,
        &format!(r#"[{{"type": "bind", "source": "{bind_src_str}", "target": "/mnt/data"}}]"#,),
    );

    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_ssh_config(&home, &[&remote]);
    let daemon = TestDaemon::start(&home);

    let config = serde_json::to_string(&serde_json::json!({"host": remote.ssh_spec()})).unwrap();
    fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mnt-bind-remote",
            "--",
            "sh",
            "-c",
            "cat /mnt/data/hello.txt && cat /mnt/data/sub/nested.txt",
        ])
        .success()
        .expect("rumpel enter failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(stdout.trim(), "from-host\nnested");
}

/// Bind mount sources containing files not owned by the current user
/// should be rejected early, before the pod is created.
#[test]
fn mount_bind_remote_rejects_foreign_owned_files() {
    // Uses an SSH-over-Docker remote; Docker executor only.
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker
    ) {
        crate::executor::skip_test();
        return;
    }
    let repo = TestRepo::new();

    // Use a system path that definitely has root-owned files.
    write_devcontainer_with_mounts(
        &repo,
        r#"[{"type": "bind", "source": "/etc", "target": "/mnt/etc"}]"#,
    );

    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_ssh_config(&home, &[&remote]);
    let daemon = TestDaemon::start(&home);

    let config = serde_json::to_string(&serde_json::json!({"host": remote.ssh_spec()})).unwrap();
    fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "mnt-bind-foreign", "--", "true"])
        .output()
        .expect("failed to execute rumpel enter");

    assert!(
        !output.status.success(),
        "rumpel enter should reject bind mount with foreign-owned files"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("owned by uid"),
        "error should mention ownership, got: {stderr}"
    );
}

/// Prepared-image cache must invalidate when a `${localEnv:...}` inside
/// a mount target resolves to a different path: the image bakes in
/// `mkdir`+`chown` for the resolved targets, so reusing an image built
/// for a different resolution would leave docker to synthesize the new
/// target as root-owned and the non-root image USER could not write to
/// the fresh volume.
#[test]
fn mount_target_localenv_change_rebuilds_image() {
    let repo = TestRepo::new();
    let vol_prefix = format!("vol-{}", repo_id(&repo));

    write_devcontainer_with_mounts(
        &repo,
        r#"[{"type": "volume",
             "source": "RUMPELPOD_TEST_MNT_${localEnv:RUMPELPOD_TEST_MNT_NAME}",
             "target": "/mnt/${localEnv:RUMPELPOD_TEST_MNT_NAME}"}]"#,
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let first_name = format!("{vol_prefix}-alpha");
    let first_stdout = pod_command(&repo, &daemon)
        .env("RUMPELPOD_TEST_MNT_NAME", &first_name)
        .args([
            "enter",
            "--create",
            "mnt-env-a",
            "--",
            "sh",
            "-c",
            &format!("echo one > /mnt/{first_name}/f && cat /mnt/{first_name}/f"),
        ])
        .success()
        .expect("first rumpel enter failed");
    assert_eq!(String::from_utf8_lossy(&first_stdout).trim(), "one");

    let second_name = format!("{vol_prefix}-beta");
    let second_stdout = pod_command(&repo, &daemon)
        .env("RUMPELPOD_TEST_MNT_NAME", &second_name)
        .args([
            "enter",
            "--create",
            "mnt-env-b",
            "--",
            "sh",
            "-c",
            &format!("echo two > /mnt/{second_name}/f && cat /mnt/{second_name}/f"),
        ])
        .success()
        .expect("second rumpel enter failed");
    assert_eq!(String::from_utf8_lossy(&second_stdout).trim(), "two");
}
