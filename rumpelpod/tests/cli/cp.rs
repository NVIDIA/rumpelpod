// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the `rumpel cp` subcommand.

use std::fs;

use rumpelpod::CommandExt;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

#[test]
fn cp_from_pod_to_host() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a file inside the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-test",
            "--",
            "sh",
            "-c",
            "echo hello-from-pod > /tmp/testfile.txt",
        ])
        .success()
        .expect("failed to create file in pod");

    // Copy it out
    let host_dest = home.path().join("copied.txt");
    pod_command(&repo, &daemon)
        .args([
            "cp",
            "cp-test:/tmp/testfile.txt",
            host_dest.to_str().unwrap(),
        ])
        .success()
        .expect("rumpel cp from pod failed");

    let content = fs::read_to_string(&host_dest).expect("failed to read copied file");
    assert_eq!(content.trim(), "hello-from-pod");
}

#[test]
fn cp_from_host_to_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Ensure pod exists
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "cp-in-test", "--", "true"])
        .success()
        .expect("failed to start pod");

    // Write a local file
    let local_file = home.path().join("upload.txt");
    fs::write(&local_file, "hello-from-host\n").expect("failed to write local file");

    // Copy it into the pod
    pod_command(&repo, &daemon)
        .args([
            "cp",
            local_file.to_str().unwrap(),
            "cp-in-test:/tmp/upload.txt",
        ])
        .success()
        .expect("rumpel cp to pod failed");

    // Verify inside the pod
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-in-test",
            "--",
            "cat",
            "/tmp/upload.txt",
        ])
        .success()
        .expect("failed to read file in pod");

    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hello-from-host");
}

#[test]
fn cp_no_pod_syntax_fails() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["cp", "/local/a", "/local/b"])
        .output()
        .expect("failed to run rumpel cp");

    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("POD:PATH"),
        "Error should mention POD:PATH syntax: {}",
        stderr
    );
}

#[test]
fn cp_both_pod_syntax_fails() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["cp", "pod1:/a", "pod2:/b"])
        .output()
        .expect("failed to run rumpel cp");

    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("both"),
        "Error should mention both sides: {}",
        stderr
    );
}

#[test]
fn cp_directory_from_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a directory with files inside the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-dir-test",
            "--",
            "sh",
            "-c",
            "mkdir -p /tmp/testdir && echo aaa > /tmp/testdir/a.txt && echo bbb > /tmp/testdir/b.txt",
        ])
        .success()
        .expect("failed to create directory in pod");

    // Copy directory out
    let host_dest = home.path().join("testdir");
    pod_command(&repo, &daemon)
        .args([
            "cp",
            "cp-dir-test:/tmp/testdir",
            host_dest.to_str().unwrap(),
        ])
        .success()
        .expect("rumpel cp directory from pod failed");

    assert!(host_dest.join("a.txt").exists(), "a.txt should exist");
    assert!(host_dest.join("b.txt").exists(), "b.txt should exist");

    let a = fs::read_to_string(host_dest.join("a.txt")).unwrap();
    assert_eq!(a.trim(), "aaa");
}

#[test]
fn cp_directory_to_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Ensure pod exists
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "cp-dir-in-test", "--", "true"])
        .success()
        .expect("failed to start pod");

    // Create a local directory with files
    let local_dir = home.path().join("upload-dir");
    fs::create_dir(&local_dir).unwrap();
    fs::write(local_dir.join("x.txt"), "xxx\n").unwrap();
    fs::write(local_dir.join("y.txt"), "yyy\n").unwrap();

    // Copy directory into the pod
    pod_command(&repo, &daemon)
        .args([
            "cp",
            local_dir.to_str().unwrap(),
            "cp-dir-in-test:/tmp/upload-dir",
        ])
        .success()
        .expect("rumpel cp directory to pod failed");

    // Verify files inside the pod
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-dir-in-test",
            "--",
            "cat",
            "/tmp/upload-dir/x.txt",
        ])
        .success()
        .expect("failed to read x.txt in pod");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "xxx");

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-dir-in-test",
            "--",
            "cat",
            "/tmp/upload-dir/y.txt",
        ])
        .success()
        .expect("failed to read y.txt in pod");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "yyy");
}

#[test]
fn cp_with_flags() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a file inside the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-flags-test",
            "--",
            "sh",
            "-c",
            "echo flagtest > /tmp/flagfile.txt",
        ])
        .success()
        .expect("failed to create file in pod");

    // Copy with -a -L flags
    let host_dest = home.path().join("flagcopy.txt");
    pod_command(&repo, &daemon)
        .args([
            "cp",
            "-a",
            "-L",
            "cp-flags-test:/tmp/flagfile.txt",
            host_dest.to_str().unwrap(),
        ])
        .success()
        .expect("rumpel cp with flags failed");

    let content = fs::read_to_string(&host_dest).expect("failed to read copied file");
    assert_eq!(content.trim(), "flagtest");
}

#[test]
fn cp_relative_path_from_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a file at a path relative to the repo root inside the pod.
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-rel-test",
            "--",
            "sh",
            "-c",
            "echo relative-content > relative-file.txt",
        ])
        .success()
        .expect("failed to create file in pod");

    // Copy it out using a relative container path (no leading /).
    let host_dest = home.path().join("rel-copied.txt");
    pod_command(&repo, &daemon)
        .args([
            "cp",
            "cp-rel-test:relative-file.txt",
            host_dest.to_str().unwrap(),
        ])
        .success()
        .expect("rumpel cp with relative path failed");

    let content = fs::read_to_string(&host_dest).expect("failed to read copied file");
    assert_eq!(content.trim(), "relative-content");
}

#[test]
fn cp_relative_path_to_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Ensure pod exists
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "cp-rel-in-test", "--", "true"])
        .success()
        .expect("failed to start pod");

    // Write a local file
    let local_file = home.path().join("rel-upload.txt");
    fs::write(&local_file, "relative-upload\n").expect("failed to write local file");

    // Copy it into the pod using a relative container path
    pod_command(&repo, &daemon)
        .args([
            "cp",
            local_file.to_str().unwrap(),
            "cp-rel-in-test:rel-upload.txt",
        ])
        .success()
        .expect("rumpel cp to pod with relative path failed");

    // Verify by reading the file at the expected absolute location
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-rel-in-test",
            "--",
            "cat",
            "rel-upload.txt",
        ])
        .success()
        .expect("failed to read file in pod");

    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "relative-upload");
}

#[test]
fn cp_directory_with_symlinks_to_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "cp-symlink-test", "--", "true"])
        .success()
        .expect("failed to start pod");

    // Build a directory containing symlinks to exercise the chown
    // path -- plain chown follows symlinks and fails with ENOENT
    // when the target has not been extracted yet.
    let local_dir = home.path().join("with-symlinks");
    let sub = local_dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("real.txt"), "hello\n").unwrap();
    std::os::unix::fs::symlink("sub/real.txt", local_dir.join("link.txt")).unwrap();
    // Dangling symlink (target does not exist on the host)
    std::os::unix::fs::symlink("nonexistent", local_dir.join("dangling")).unwrap();

    pod_command(&repo, &daemon)
        .args([
            "cp",
            local_dir.to_str().unwrap(),
            "cp-symlink-test:/tmp/symlinked",
        ])
        .success()
        .expect("rumpel cp directory with symlinks failed");

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-symlink-test",
            "--",
            "cat",
            "/tmp/symlinked/sub/real.txt",
        ])
        .success()
        .expect("failed to read real.txt in pod");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hello");

    // The symlink itself should exist (readlink should work)
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-symlink-test",
            "--",
            "readlink",
            "/tmp/symlinked/link.txt",
        ])
        .success()
        .expect("failed to readlink in pod");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "sub/real.txt");
}

#[test]
#[ignore] // Requires network access and clones ~400 MB from GitHub.
fn cp_large_directory_to_pod() {
    // Clone a large repository and copy it into a pod to exercise the
    // streaming tar/gzip upload path with a realistic payload.
    let clone_path = std::path::Path::new("/tmp/rumpelpod-rust-clone");
    if !clone_path.exists() {
        std::process::Command::new("git")
            .args([
                "clone",
                "--depth=1",
                "https://github.com/rust-lang/rust.git",
                clone_path.to_str().unwrap(),
            ])
            .status()
            .expect("failed to spawn git clone")
            .success()
            .then_some(())
            .expect("git clone failed");
    }

    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "cp-large-test", "--", "true"])
        .success()
        .expect("failed to start pod");

    pod_command(&repo, &daemon)
        .args([
            "cp",
            clone_path.to_str().unwrap(),
            "cp-large-test:/tmp/rust",
        ])
        .success()
        .expect("rumpel cp large directory to pod failed");

    // Spot-check: the Cargo.toml at the repo root should exist
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-large-test",
            "--",
            "head",
            "-1",
            "/tmp/rust/Cargo.toml",
        ])
        .success()
        .expect("failed to read Cargo.toml in pod");

    let first_line = String::from_utf8_lossy(&stdout);
    assert!(
        first_line.contains("[workspace]") || first_line.contains("[package]"),
        "Expected Cargo.toml header, got: {first_line}"
    );
}

#[test]
fn cp_to_pod_owns_files_as_container_user() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Ensure pod exists
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "cp-own-test", "--", "true"])
        .success()
        .expect("failed to start pod");

    // Write a local file
    let local_file = home.path().join("owned.txt");
    fs::write(&local_file, "check-owner\n").expect("failed to write local file");

    // Copy into the pod (without -a, so chown should kick in)
    pod_command(&repo, &daemon)
        .args([
            "cp",
            local_file.to_str().unwrap(),
            "cp-own-test:/tmp/owned.txt",
        ])
        .success()
        .expect("rumpel cp to pod failed");

    // Check the file owner matches the container user
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-own-test",
            "--",
            "stat",
            "-c",
            "%U",
            "/tmp/owned.txt",
        ])
        .success()
        .expect("failed to stat file in pod");

    let owner = String::from_utf8_lossy(&stdout).trim().to_string();
    // The default test image runs as a non-root user; the file should not be root.
    assert_ne!(
        owner, "root",
        "File should be owned by the container user, not root"
    );
}

/// `cp file pod:/existing-dir` should place the file inside the directory,
/// matching standard cp behavior.
#[test]
fn cp_file_to_existing_directory_in_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a directory inside the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-fdir-test",
            "--",
            "mkdir",
            "-p",
            "/tmp/destdir",
        ])
        .success()
        .expect("failed to create directory in pod");

    // Write a local file
    let local_file = home.path().join("myfile.txt");
    fs::write(&local_file, "file-in-dir\n").unwrap();

    // Copy file into the pod, specifying the directory as destination
    pod_command(&repo, &daemon)
        .args([
            "cp",
            local_file.to_str().unwrap(),
            "cp-fdir-test:/tmp/destdir",
        ])
        .success()
        .expect("rumpel cp file to existing dir failed");

    // The file should appear inside the directory, not replace it
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-fdir-test",
            "--",
            "cat",
            "/tmp/destdir/myfile.txt",
        ])
        .success()
        .expect("file should be inside the directory");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "file-in-dir");
}

/// `cp pod:/file /existing-dir` should place the file inside the directory.
#[test]
fn cp_file_from_pod_to_existing_directory() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a file inside the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-fdir-from-test",
            "--",
            "sh",
            "-c",
            "echo from-pod > /tmp/podfile.txt",
        ])
        .success()
        .expect("failed to create file in pod");

    // Create a local directory as destination
    let local_dir = home.path().join("localdir");
    fs::create_dir(&local_dir).unwrap();

    // Copy from pod into the existing local directory
    pod_command(&repo, &daemon)
        .args([
            "cp",
            "cp-fdir-from-test:/tmp/podfile.txt",
            local_dir.to_str().unwrap(),
        ])
        .success()
        .expect("rumpel cp file from pod to existing dir failed");

    // The file should be placed inside the directory
    let content = fs::read_to_string(local_dir.join("podfile.txt"))
        .expect("file should be inside the directory");
    assert_eq!(content.trim(), "from-pod");
}

/// `cp dir pod:/existing-dir` should nest the source directory inside the
/// destination, matching `cp -r src dst/` when dst exists.
#[test]
fn cp_directory_to_existing_directory_in_pod() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a directory inside the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-ddir-test",
            "--",
            "mkdir",
            "-p",
            "/tmp/parent",
        ])
        .success()
        .expect("failed to create directory in pod");

    // Create a local directory with a file
    let local_dir = home.path().join("subdir");
    fs::create_dir(&local_dir).unwrap();
    fs::write(local_dir.join("nested.txt"), "nested\n").unwrap();

    // Copy directory into the pod, targeting the existing directory
    pod_command(&repo, &daemon)
        .args([
            "cp",
            local_dir.to_str().unwrap(),
            "cp-ddir-test:/tmp/parent",
        ])
        .success()
        .expect("rumpel cp dir to existing dir failed");

    // The source directory should be nested: /tmp/parent/subdir/nested.txt
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-ddir-test",
            "--",
            "cat",
            "/tmp/parent/subdir/nested.txt",
        ])
        .success()
        .expect("directory should be nested inside the destination");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "nested");
}

/// `cp pod:/dir /existing-dir` should nest the source directory inside the
/// local destination.
#[test]
fn cp_directory_from_pod_to_existing_directory() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a directory with files inside the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cp-ddir-from-test",
            "--",
            "sh",
            "-c",
            "mkdir -p /tmp/srcdir && echo aaa > /tmp/srcdir/a.txt",
        ])
        .success()
        .expect("failed to create directory in pod");

    // Create a local directory as destination
    let local_dir = home.path().join("existing");
    fs::create_dir(&local_dir).unwrap();

    // Copy directory from pod into the existing local directory
    pod_command(&repo, &daemon)
        .args([
            "cp",
            "cp-ddir-from-test:/tmp/srcdir",
            local_dir.to_str().unwrap(),
        ])
        .success()
        .expect("rumpel cp dir from pod to existing dir failed");

    // The source directory should be nested: existing/srcdir/a.txt
    let content = fs::read_to_string(local_dir.join("srcdir").join("a.txt"))
        .expect("directory should be nested inside the destination");
    assert_eq!(content.trim(), "aaa");
}

#[test]
fn cp_nonexistent_pod_fails() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["cp", "no-such-pod:/tmp/file.txt", "/tmp/out.txt"])
        .output()
        .expect("failed to run rumpel cp");

    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist"),
        "should report pod does not exist: {stderr}",
    );
}
