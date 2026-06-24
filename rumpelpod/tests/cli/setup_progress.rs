// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that container-serve setup progress and errors are visible
//! to the CLI client that triggered the pod launch.
//!
//! Before the /events-early change, setup ran before the server
//! accepted connections, so errors were invisible (lost in the
//! detached exec's stderr).  Now setup streams progress through
//! /events as SSE `event: progress` messages.

use indoc::formatdoc;
use std::fs;

use crate::common::{pod_command, TestDaemon, TestHome, TestRepo, TEST_REPO_PATH, TEST_USER};
use crate::executor::ExecutorResources;

fn write_devcontainer_with_lifecycle(repo: &TestRepo, lifecycle_json: &str) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("create .devcontainer");

    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git shadow
        RUN useradd -m -u 1000 {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
    "#};
    fs::write(devcontainer_dir.join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": ".."
            }},
            "workspaceFolder": "{TEST_REPO_PATH}",
            "containerUser": "{TEST_USER}",
            "runArgs": ["--runtime=runc"],
            {lifecycle_json}
        }}
    "#};
    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("write devcontainer.json");
}

/// A failing onCreateCommand should produce an error message visible
/// in the CLI's stderr, not silently swallowed inside the container.
#[test]
fn lifecycle_failure_visible_in_client_stderr() {
    let repo = TestRepo::new();
    write_devcontainer_with_lifecycle(
        &repo,
        r#""onCreateCommand": "echo MARKER_LIFECYCLE_FAIL >&2; exit 1""#,
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "lc-visible", "--", "echo", "ok"])
        .output()
        .expect("failed to run rumpel enter");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // The lifecycle failure message should reach the client.
    assert!(
        stderr.contains("onCreateCommand failed"),
        "client stderr should mention the failing lifecycle command.\nstderr: {stderr}"
    );
}

/// Setup progress messages (env probe, git setup) should be visible
/// in the client's stderr during pod creation.
#[test]
fn setup_progress_visible_in_client_stderr() {
    let repo = TestRepo::new();
    write_devcontainer_with_lifecycle(&repo, r#""onCreateCommand": "true""#);
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "progress-vis", "--", "echo", "ok"])
        .output()
        .expect("failed to run rumpel enter");

    assert!(output.status.success(), "enter should succeed");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // The daemon may connect to /events after the first few progress
    // messages have already been sent (broadcast drops messages with
    // no subscribers).  Git setup and lifecycle messages arrive later
    // and should be visible.
    assert!(
        stderr.contains("setting up git remotes")
            || stderr.contains("running lifecycle commands")
            || stderr.contains("resolving environment"),
        "client stderr should show at least one setup progress step.\nstderr: {stderr}"
    );
}

/// When the tunnel port (7891) is already occupied inside the
/// container, git setup fails.  The error should be visible to the
/// client, not silently swallowed.
///
/// Currently this test hangs because git fetch has no timeout and
/// the occupied port accepts connections (socat) but doesn't speak
/// the tunnel protocol.  TODO: add timeouts to git operations in
/// run_setup so the setup task fails instead of hanging.
#[test]
#[ignore]
fn git_setup_failure_visible_in_client_stderr() {
    let repo = TestRepo::new();
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("create .devcontainer");

    // The Dockerfile's CMD occupies port 7891 (the tunnel port) so
    // the tunnel-server cannot bind and git fetch fails.
    // overrideCommand:false keeps the CMD running as PID 1.
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git shadow coreutils bash socat openssh-client
        RUN useradd -m -u 1000 {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        USER {TEST_USER}
        CMD ["socat", "TCP-LISTEN:7891,fork,reuseaddr", "SYSTEM:true"]
    "#};
    fs::write(devcontainer_dir.join("Dockerfile"), &dockerfile).expect("write Dockerfile");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": ".."
            }},
            "workspaceFolder": "{TEST_REPO_PATH}",
            "containerUser": "{TEST_USER}",
            "runArgs": ["--runtime=runc"],
            "overrideCommand": false
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

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "git-fail", "--", "echo", "ok"])
        .output()
        .expect("failed to run rumpel enter");

    // Enter should fail because git setup cannot reach the tunnel.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "enter should fail when git setup fails.\nstderr: {stderr}"
    );
    // The error should mention git or fetch so the user knows what
    // went wrong.
    assert!(
        stderr.contains("git") || stderr.contains("fetch") || stderr.contains("tunnel"),
        "client stderr should mention the git/tunnel failure.\nstderr: {stderr}"
    );
}
