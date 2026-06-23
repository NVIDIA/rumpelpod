// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for devcontainer.json image building.

use std::fs;
use std::process::Command;

use indoc::formatdoc;
use rumpelpod::CommandExt;

use crate::common::{
    create_commit, pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo,
    TEST_REPO_PATH, TEST_USER, TEST_USER_UID,
};
use crate::executor::ExecutorResources;

/// Create a test devcontainer.json with a build configuration.
fn write_devcontainer_with_build(repo: &TestRepo, dockerfile_name: &str) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("Failed to create .devcontainer directory");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "{dockerfile_name}",
                "context": ".."
            }},
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

/// Create a simple test Dockerfile.
fn write_test_dockerfile(repo: &TestRepo, content: &str) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("Failed to create .devcontainer directory");

    fs::write(devcontainer_dir.join("Dockerfile"), content).expect("Failed to write Dockerfile");
}

/// Create a test devcontainer.json with an `image` reference (no build).
fn write_devcontainer_with_image(repo: &TestRepo, image: &str) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("Failed to create .devcontainer directory");

    let devcontainer_json = formatdoc! {r#"
        {{
            "image": "{image}",
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

/// Build a rumpel Command that does not need a daemon.
///
/// `image build` and `image fetch` call Docker directly, so no daemon socket
/// is required.
fn rumpel_cmd(repo: &TestRepo) -> Command {
    let mut cmd = Command::new("rumpel");
    cmd.current_dir(repo.path());
    cmd
}

#[test]
fn devcontainer_build_simple() {
    let repo = TestRepo::new();

    // Create a simple Dockerfile that copies the repo
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Enter the pod - this should trigger an image build
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "build-test",
            "--",
            "echo",
            "hello from built image",
        ])
        .success()
        .expect("rumpel enter failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(stdout.trim(), "hello from built image");
}

#[test]
fn devcontainer_existing_checkout_preserves_mtime() {
    let repo = TestRepo::new();
    let fixture = "mtime-sensitive.txt";
    fs::write(repo.path().join(fixture), "mtime survives pod setup\n")
        .expect("writing mtime fixture");
    Command::new("git")
        .args(["add", fixture])
        .current_dir(repo.path())
        .success()
        .expect("git add mtime fixture failed");
    create_commit(repo.path(), "Add mtime fixture");

    let extra_dockerfile = formatdoc! {r#"
        RUN touch -d @946684800 {TEST_REPO_PATH}/{fixture}
        RUN stat -c %Y {TEST_REPO_PATH}/{fixture} > /tmp/baked-mtime
    "#};
    write_test_devcontainer(&repo, &extra_dockerfile, "");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let command = formatdoc! {r#"
        set -eu
        baked="$(cat /tmp/baked-mtime)"
        current="$(stat -c %Y {TEST_REPO_PATH}/{fixture})"
        printf '%s\n%s\n' "$baked" "$current"
    "#};
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mtime-preserved",
            "--",
            "sh",
            "-c",
            &command,
        ])
        .output()
        .expect("rumpel enter failed to run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "rumpel enter failed while checking mtimes; stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    let mtimes: Vec<_> = stdout.lines().collect();
    assert_eq!(
        mtimes.len(),
        2,
        "expected baked and current mtimes; stdout:\n{stdout}",
    );
    assert_eq!(
        mtimes[0], mtimes[1],
        "mtime changed across pod startup; stdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

#[test]
fn devcontainer_existing_checkout_preserves_mtime_after_host_commit() {
    let repo = TestRepo::new();
    let stable = "stable-mtime.txt";
    let changed = "changed-after-image.txt";
    fs::write(repo.path().join(stable), "stable across host commits\n")
        .expect("writing stable fixture");
    fs::write(repo.path().join(changed), "baked content\n").expect("writing changed fixture");
    Command::new("git")
        .args(["add", stable, changed])
        .current_dir(repo.path())
        .success()
        .expect("git add fixtures failed");
    create_commit(repo.path(), "Add mtime fixtures");

    let unique = repo
        .path()
        .file_name()
        .expect("repo path should have a name")
        .to_string_lossy()
        .to_ascii_lowercase();
    let image_tag = format!("rumpelpod-mtime-fetch-{unique}:latest");
    let dockerfile_dir = tempfile::tempdir().expect("creating Dockerfile temp dir");
    let dockerfile_path = dockerfile_dir.path().join("Dockerfile");
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git bash shadow coreutils
        RUN useradd -m -u {TEST_USER_UID} -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        RUN git config --global --add safe.directory {TEST_REPO_PATH}
        RUN touch -d @946684800 {TEST_REPO_PATH}/{stable}
        RUN stat -c %Y {TEST_REPO_PATH}/{stable} > /tmp/baked-stable-mtime
        USER {TEST_USER}
    "#};
    fs::write(&dockerfile_path, dockerfile).expect("writing Dockerfile");
    Command::new("docker")
        .args(["build", "-t", &image_tag, "-f"])
        .arg(&dockerfile_path)
        .arg(repo.path())
        .success()
        .expect("docker build failed");

    write_devcontainer_with_image(&repo, &image_tag);

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let command = formatdoc! {r#"
        set -eu
        baked="$(cat /tmp/baked-stable-mtime)"
        current="$(stat -c %Y {TEST_REPO_PATH}/{stable})"
        printf '%s\n%s\n' "$baked" "$current"
    "#};
    let first_output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mtime-fetch-base",
            "--",
            "sh",
            "-c",
            &command,
        ])
        .output()
        .expect("first rumpel enter failed to run");
    let first_stdout = String::from_utf8_lossy(&first_output.stdout);
    let first_stderr = String::from_utf8_lossy(&first_output.stderr);
    assert!(
        first_output.status.success(),
        "first rumpel enter failed; stdout:\n{first_stdout}\nstderr:\n{first_stderr}",
    );
    let first_mtimes: Vec<_> = first_stdout.lines().collect();
    assert_eq!(
        first_mtimes.len(),
        2,
        "expected baked and current mtimes; stdout:\n{first_stdout}",
    );
    assert_eq!(
        first_mtimes[0], first_mtimes[1],
        "mtime changed while building the initial prepared image; stdout:\n{first_stdout}\nstderr:\n{first_stderr}",
    );

    fs::write(repo.path().join(changed), "changed after image build\n")
        .expect("updating changed fixture");
    Command::new("git")
        .args(["add", changed])
        .current_dir(repo.path())
        .success()
        .expect("git add changed fixture failed");
    create_commit(repo.path(), "Change B after image build");

    let command = formatdoc! {r#"
        set -eu
        baked="$(cat /tmp/baked-stable-mtime)"
        current="$(stat -c %Y {TEST_REPO_PATH}/{stable})"
        changed="$(cat {TEST_REPO_PATH}/{changed})"
        printf '%s\n%s\n%s\n' "$baked" "$current" "$changed"
    "#};
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "mtime-fetch-later",
            "--",
            "sh",
            "-c",
            &command,
        ])
        .output()
        .expect("second rumpel enter failed to run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "second rumpel enter failed; stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "expected baked mtime, current mtime, and changed file content; stdout:\n{stdout}",
    );
    assert_eq!(
        lines[0], lines[1],
        "mtime changed while fetching a newer host commit; stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert_eq!(
        lines[2], "changed after image build",
        "new pod did not fetch the host commit that changed only {changed}; stdout:\n{stdout}",
    );
}

#[test]
fn devcontainer_build_with_args() {
    let repo = TestRepo::new();

    // Create a Dockerfile that uses build args
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        ARG TEST_VALUE=default
        RUN apk add --no-cache git bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        RUN echo "BUILD_ARG: $TEST_VALUE" > /tmp/build-arg.txt
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);

    // Create devcontainer.json with build args
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("Failed to create .devcontainer directory");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": "..",
                "args": {{
                    "TEST_VALUE": "custom_value"
                }}
            }},
            "workspaceFolder": "{TEST_REPO_PATH}",
            "containerUser": "{TEST_USER}"
        }}
    "#};

    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("Failed to write devcontainer.json");

    let home = TestHome::new();

    let executor = ExecutorResources::setup(&home);

    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Enter the pod and check that the build arg was used
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "args-test",
            "--",
            "cat",
            "/tmp/build-arg.txt",
        ])
        .success()
        .expect("rumpel enter failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(stdout.trim(), "BUILD_ARG: custom_value");
}

#[test]
fn devcontainer_build_with_target() {
    let repo = TestRepo::new();

    // Create a multi-stage Dockerfile
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base AS base
        RUN apk add --no-cache git bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}

        FROM base AS development
        RUN echo "development stage" > /tmp/stage.txt
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}

        FROM base AS production
        RUN echo "production stage" > /tmp/stage.txt
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);

    // Create devcontainer.json targeting the development stage
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("Failed to create .devcontainer directory");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": "..",
                "target": "development"
            }},
            "workspaceFolder": "{TEST_REPO_PATH}",
            "containerUser": "{TEST_USER}"
        }}
    "#};

    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("Failed to write devcontainer.json");

    let home = TestHome::new();

    let executor = ExecutorResources::setup(&home);

    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Enter the pod and verify we're using the development stage
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "target-test",
            "--",
            "cat",
            "/tmp/stage.txt",
        ])
        .success()
        .expect("rumpel enter failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(stdout.trim(), "development stage");
}

#[test]
fn devcontainer_build_reuses_cached_image() {
    let repo = TestRepo::new();

    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // First enter - should build the image
    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "--create", "cache-test-1", "--", "echo", "first"])
        .success()
        .expect("first rumpel enter failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "first");

    // Second enter with different pod name - should reuse the cached image
    // (same Dockerfile hash) without rebuilding
    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "--create", "cache-test-2", "--", "echo", "second"])
        .success()
        .expect("second rumpel enter failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "second");

    // Verify both pods can run commands (they're using the same image)
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cache-test-1",
            "--",
            "echo",
            "still works",
        ])
        .success()
        .expect("reentry to first pod failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "still works");
}

#[test]
fn devcontainer_build_rebuilds_when_context_file_changes() {
    let repo = TestRepo::new();

    // Drop a marker into the build context whose content the image
    // bakes in via COPY.  If context hashing is working, modifying
    // the marker must invalidate the cached image so the second
    // enter rebuilds and the container sees the new bytes.
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("create .devcontainer dir");
    fs::write(devcontainer_dir.join("marker.txt"), "version-1").expect("write marker v1");

    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY marker.txt /marker.txt
        USER {TEST_USER}
    "#};
    fs::write(devcontainer_dir.join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": "."
            }},
            "workspaceFolder": "{TEST_REPO_PATH}",
            "containerUser": "{TEST_USER}"
        }}
    "#};
    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("write devcontainer.json");

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "context-change-1",
            "--",
            "cat",
            "/marker.txt",
        ])
        .success()
        .expect("first enter failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "version-1");

    fs::write(devcontainer_dir.join("marker.txt"), "version-2").expect("write marker v2");

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "context-change-2",
            "--",
            "cat",
            "/marker.txt",
        ])
        .success()
        .expect("second enter failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "version-2");
}

#[test]
fn devcontainer_build_with_context() {
    // Test that context is correctly resolved relative to devcontainer.json
    let repo = TestRepo::new();

    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);

    // First, use context ".." (repo root)
    write_devcontainer_with_build(&repo, "Dockerfile");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "context-test",
            "--",
            "echo",
            "build works",
        ])
        .success()
        .expect("rumpel enter failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "build works");
}

#[test]
fn devcontainer_build_skips_when_image_exists() {
    let repo = TestRepo::new();

    let unique = repo.path().file_name().unwrap().to_string_lossy();
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        LABEL test.unique="{unique}"
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // First enter should build the image and stream build output.
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "skip-test-1", "--", "echo", "built"])
        .output()
        .expect("first pod enter failed");
    assert!(output.status.success(), "first enter failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("FROM") || stderr.contains("Step") || stderr.contains("RUN"),
        "first enter should stream build output, stderr: {stderr}",
    );

    // Second enter with a different pod name should reuse the cached image.
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "skip-test-2", "--", "echo", "skipped"])
        .output()
        .expect("second pod enter failed");
    assert!(output.status.success(), "second enter failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !(stderr.contains("FROM") || stderr.contains("Step") || stderr.contains("RUN")),
        "second enter should skip the build, stderr: {stderr}",
    );
}

// ---- rumpel image build / fetch subcommand tests ----

#[test]
fn image_build_builds_and_reports() {
    let repo = TestRepo::new();

    // Embed the repo temp-dir name so the Dockerfile hash is unique across
    // test runs and does not collide with cached images from other tests.
    let unique = repo.path().file_name().unwrap().to_string_lossy();
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        LABEL test.unique="{unique}"
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");

    let stdout = rumpel_cmd(&repo)
        .args(["image", "build"])
        .success()
        .expect("rumpel image build failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        stdout.contains("Image built:"),
        "expected 'Image built:' in output, got: {stdout}",
    );
}

#[test]
fn image_build_always_rebuilds() {
    let repo = TestRepo::new();

    let unique = repo.path().file_name().unwrap().to_string_lossy();
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        LABEL test.unique="{unique}"
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");

    // First build
    rumpel_cmd(&repo)
        .args(["image", "build"])
        .success()
        .expect("first image build failed");

    // Second build -- should rebuild, not report cached
    let stdout = rumpel_cmd(&repo)
        .args(["image", "build"])
        .success()
        .expect("second image build failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        stdout.contains("Image built:"),
        "expected 'Image built:' on second run, got: {stdout}",
    );
}

#[test]
fn image_build_errors_on_image_only_config() {
    let repo = TestRepo::new();
    write_devcontainer_with_image(&repo, "cgr.dev/chainguard/wolfi-base");

    let output = rumpel_cmd(&repo)
        .args(["image", "build"])
        .output()
        .expect("failed to run rumpel");

    assert!(
        !output.status.success(),
        "image build should fail for image-only config"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("image fetch"),
        "error should suggest 'image fetch', got: {stderr}",
    );
}

#[test]
fn image_fetch_pulls_image() {
    let repo = TestRepo::new();
    // hello-world is tiny and fast to pull
    write_devcontainer_with_image(&repo, "hello-world:latest");

    let stdout = rumpel_cmd(&repo)
        .args(["image", "fetch"])
        .success()
        .expect("rumpel image fetch failed");

    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        stdout.contains("Image pulled:"),
        "expected 'Image pulled:' in output, got: {stdout}",
    );
}

#[test]
fn image_fetch_errors_on_build_config() {
    let repo = TestRepo::new();

    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");

    let output = rumpel_cmd(&repo)
        .args(["image", "fetch"])
        .output()
        .expect("failed to run rumpel");

    assert!(
        !output.status.success(),
        "image fetch should fail for build-based config"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("image build"),
        "error should suggest 'image build', got: {stderr}",
    );
}

// ---- default image tests ----

#[test]
fn no_devcontainer_enters_with_default_image() {
    let repo = TestRepo::new();
    // No devcontainer.json -- should build the default image and enter.
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "default-image-test",
            "--",
            "echo",
            "hello from default",
        ])
        .output()
        .expect("rumpel enter failed");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no image or build configured, building default image"),
        "expected default-image warning, got: {stderr}",
    );

    assert!(output.status.success(), "enter failed, stderr: {stderr}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "hello from default");
}

/// Even when the daemon cannot see a host codex binary, prepared-image
/// should still inject the rumpelpod prompt into ~/.codex/AGENTS.md if
/// the devcontainer image already contains codex.
#[test]
fn base_image_with_codex_gets_agents_prompt_without_host_codex() {
    let repo = TestRepo::new();
    write_test_devcontainer(
        &repo,
        "RUN mkdir -p /usr/local/bin && printf '#!/bin/sh\\necho codex-cli test\\n' > /usr/local/bin/codex && chmod +x /usr/local/bin/codex",
        "",
    );

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "base-image-codex-test",
            "--",
            "sh",
            "-c",
            "test -f \"$HOME/.codex/AGENTS.md\" && grep -F DESCRIPTION \"$HOME/.codex/AGENTS.md\"",
        ])
        .output()
        .expect("rumpel enter failed");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "~/.codex/AGENTS.md should exist when codex is already in the image, stderr: {stderr}",
    );
}

// ---- missing git tests ----

#[test]
fn devcontainer_build_errors_when_git_missing() {
    let repo = TestRepo::new();

    // Dockerfile that deliberately omits git.
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "no-git-test", "--", "true"])
        .output()
        .expect("rumpel enter failed to run");

    assert!(
        !output.status.success(),
        "enter should fail when git is missing"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not have git installed"),
        "error should mention git is missing, got: {stderr}",
    );
}

// ---- streaming build output tests ----

#[test]
fn devcontainer_build_streams_output() {
    let repo = TestRepo::new();

    let unique = repo.path().file_name().unwrap().to_string_lossy();
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git bash shadow
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        LABEL test.stream="{unique}"
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "stream-test", "--", "echo", "streamed"])
        .output()
        .expect("rumpel enter failed");

    assert!(output.status.success(), "enter failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("FROM") || stderr.contains("Step") || stderr.contains("RUN"),
        "build output should be streamed to stderr, got: {stderr}",
    );
    // The prepared-image step runs `rumpel prepare-image` in a RUN
    // layer -- its presence in stderr proves that step is also being
    // streamed, not just the base image build.
    assert!(
        stderr.contains("prepare-image"),
        "prepared image build output should be streamed to stderr, got: {stderr}",
    );
}

#[test]
fn image_build_streams_output() {
    let repo = TestRepo::new();

    let unique = repo.path().file_name().unwrap().to_string_lossy();
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        LABEL test.stream.direct="{unique}"
    "#};

    write_test_dockerfile(&repo, &dockerfile);
    write_devcontainer_with_build(&repo, "Dockerfile");

    let output = rumpel_cmd(&repo)
        .args(["image", "build"])
        .output()
        .expect("rumpel image build failed");

    assert!(output.status.success(), "image build failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("FROM") || stderr.contains("Step") || stderr.contains("RUN"),
        "build output should be streamed to stderr, got: {stderr}",
    );
}

/// Verify `--ssh=default` in `build.options` reaches the client's
/// ssh-agent even when the rumpel daemon has no agent of its own.
///
/// Scrubs `SSH_AUTH_SOCK` from the test process before the daemon
/// starts (so the daemon inherits a clean env), then starts a fresh
/// ssh-agent that only the `rumpel enter` subprocess sees.  The
/// Dockerfile queries the forwarded agent with `ssh-add -l` at build
/// time and writes the output to the image; the test reads it back
/// from inside the container and asserts the fingerprint matches.
#[test]
fn devcontainer_build_forwards_client_ssh_agent() {
    let repo = TestRepo::new();

    // Each xtest case runs in its own process, so mutating env here
    // cannot race with other tests.  The daemon we spawn below must
    // NOT inherit a usable SSH_AUTH_SOCK: otherwise its own agent
    // would satisfy --ssh=default regardless of whether forwarding
    // works.
    std::env::remove_var("SSH_AUTH_SOCK");

    // Embed the temp repo's unique name in a LABEL so each test run
    // hashes to a fresh image tag: the image hash ignores the agent
    // socket and the key set, so without a per-run cache buster the
    // cached image from a prior run would satisfy `ssh-add -l` with
    // yesterday's fingerprint.
    let unique = repo.path().file_name().unwrap().to_string_lossy();
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        LABEL test.ssh-forward="{unique}"
        RUN apk add --no-cache git bash shadow openssh-client
        RUN useradd -m -u 1007 -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        # `ssh-add -l` fails loudly (non-zero exit) if the agent is
        # unreachable, so a missing forward aborts the build rather
        # than silently producing an image with empty output.
        RUN --mount=type=ssh ssh-add -l > /tmp/ssh-keys.txt
        USER {TEST_USER}
    "#};

    write_test_dockerfile(&repo, &dockerfile);

    let devcontainer_dir = repo.path().join(".devcontainer");
    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": "..",
                "options": ["--ssh=default"]
            }},
            "workspaceFolder": "{TEST_REPO_PATH}",
            "containerUser": "{TEST_USER}"
        }}
    "#};
    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("writing devcontainer.json");

    let home = TestHome::new();
    home.link_local_bins(&["ssh-keygen", "ssh-agent", "ssh-add"]);
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Start a throwaway ssh-agent that the daemon never sees; only
    // the `rumpel enter` client gets SSH_AUTH_SOCK pointed at it.
    let agent_sock = home.path().join("client-agent.sock");
    let agent_out = Command::new("ssh-agent")
        .args(["-a"])
        .arg(&agent_sock)
        .output()
        .expect("ssh-agent failed");
    assert!(
        agent_out.status.success(),
        "ssh-agent failed: {}",
        String::from_utf8_lossy(&agent_out.stderr)
    );
    let agent_stdout = String::from_utf8_lossy(&agent_out.stdout);
    let agent_pid: u32 = agent_stdout
        .lines()
        .find_map(|l| {
            l.strip_prefix("SSH_AGENT_PID=")
                .and_then(|s| s.split(';').next())
        })
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("parsing agent PID from: {agent_stdout}"));

    let key_path = home.path().join("build-key");
    Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f"])
        .arg(&key_path)
        .success()
        .expect("ssh-keygen failed");
    Command::new("ssh-add")
        .env("SSH_AUTH_SOCK", &agent_sock)
        .arg(&key_path)
        .success()
        .expect("ssh-add failed");

    // Pull out the fingerprint `ssh-add -l` will emit, so the
    // assertion below matches the exact key we loaded and not some
    // unrelated identity leaked by a pre-existing agent.
    let fp_out = Command::new("ssh-keygen")
        .args(["-lf"])
        .arg(&key_path)
        .output()
        .expect("ssh-keygen -l failed");
    let fp_stdout = String::from_utf8_lossy(&fp_out.stdout);
    let expected_fp = fp_stdout
        .split_whitespace()
        .nth(1)
        .expect("parsing fingerprint")
        .to_string();

    let output = pod_command(&repo, &daemon)
        .env("SSH_AUTH_SOCK", &agent_sock)
        .args([
            "enter",
            "--create",
            "ssh-build",
            "--",
            "cat",
            "/tmp/ssh-keys.txt",
        ])
        .output()
        .expect("rumpel enter failed");

    // Shut down the throwaway agent regardless of the assertion
    // outcome so it does not linger past the test process.
    let _ = Command::new("ssh-agent")
        .arg("-k")
        .env("SSH_AUTH_SOCK", &agent_sock)
        .env("SSH_AGENT_PID", agent_pid.to_string())
        .output();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "enter failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains(&expected_fp),
        "expected fingerprint {expected_fp} in /tmp/ssh-keys.txt, got: {stdout}"
    );
}
