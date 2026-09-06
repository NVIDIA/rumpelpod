// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::process::Command;

use indoc::{formatdoc, indoc};
use rumpelpod::config::load_json_config;
use rumpelpod::CommandExt;
use serde_json::json;

use crate::common::{
    create_commit, pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo,
    TEST_REPO_PATH, TEST_USER,
};
use crate::executor::{merge_config, ExecutorResources};

#[test]
fn repo_clone_modes_control_baking_without_changing_startup_sync() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("tracked.txt"), "available at startup\n").unwrap();
    Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(repo.path())
        .success()
        .unwrap();
    create_commit(repo.path(), "Provide content for startup fetch");
    let expected_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .success()
        .unwrap();
    Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://example.invalid/project.git",
        ])
        .current_dir(repo.path())
        .success()
        .unwrap();

    // The entrypoint observes the image before rumpelpod initializes Git.
    // Keeping the context in .devcontainer also prevents the base build
    // from accidentally supplying the checkout under test.
    let devcontainer = repo.path().join(".devcontainer");
    fs::create_dir(&devcontainer).unwrap();
    fs::write(
        devcontainer.join("entrypoint.sh"),
        formatdoc! {r#"
            #!/bin/sh
            set -eu
            if test -e {TEST_REPO_PATH}/.git; then
                echo local > /tmp/baked-repo-mode
            else
                echo skip > /tmp/baked-repo-mode
            fi
            exec "$@"
        "#},
    )
    .unwrap();
    fs::write(
        devcontainer.join("Dockerfile"),
        formatdoc! {r#"
            FROM cgr.dev/chainguard/wolfi-base
            RUN apk add --no-cache git bash shadow
            RUN useradd -m -s /bin/bash {TEST_USER}
            RUN git config --system init.defaultBranch skip
            COPY entrypoint.sh /usr/local/bin/repo-state
            ENTRYPOINT ["sh", "/usr/local/bin/repo-state"]
            USER {TEST_USER}
        "#},
    )
    .unwrap();
    fs::write(
        devcontainer.join("devcontainer.json"),
        formatdoc! {r#"
            {{
                "build": {{"dockerfile": "Dockerfile", "context": "."}},
                "workspaceFolder": "{TEST_REPO_PATH}",
                "onCreateCommand": "test -f tracked.txt && git rev-parse HEAD > /tmp/lifecycle-head"
            }}
        "#},
    )
    .unwrap();

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    // Changing just repoClone must not reuse a prepared image from the
    // other mode, while an omitted setting keeps the existing default.
    for (name, settings, baked_mode) in [
        ("default", json!({}), "local\n"),
        (
            "skip",
            json!({"build": {"repoClone": {"mode": "skip"}}}),
            "skip\n",
        ),
        (
            "local",
            json!({"build": {"repoClone": {"mode": "local"}}}),
            "local\n",
        ),
    ] {
        fs::write(
            repo.path().join(".rumpelpod.json"),
            merge_config(&executor.json, settings),
        )
        .unwrap();
        let output = pod_command(&repo, &daemon)
            .args([
                "enter",
                "--create",
                name,
                "--",
                "cat",
                "/tmp/baked-repo-mode",
            ])
            .success()
            .expect("launch pod with the selected clone mode");
        assert_eq!(String::from_utf8(output).unwrap(), baked_mode);

        let output = pod_command(&repo, &daemon)
            .args(["enter", name, "--", "cat", "/tmp/lifecycle-head"])
            .success()
            .unwrap();
        assert_eq!(output, expected_head);
        let output = pod_command(&repo, &daemon)
            .args(["enter", name, "--", "git", "remote", "get-url", "origin"])
            .success()
            .unwrap();
        assert_eq!(output, b"https://example.invalid/project.git\n");

        let rejected = pod_command(&repo, &daemon)
            .args([
                "enter",
                name,
                "--",
                "git",
                "commit",
                "--allow-empty",
                "-m",
                "Needs a description",
            ])
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("DESCRIPTION is not staged"));

        let output = pod_command(&repo, &daemon)
            .args([
                "enter", name, "--", "sh", "-c",
                "git commit --allow-empty --no-verify -m 'Publish pod work' >&2 && git rev-parse HEAD",
            ])
            .success()
            .unwrap();
        let published = Command::new("git")
            .args(["rev-parse", &format!("refs/rumpelpod/{name}")])
            .current_dir(repo.path())
            .success()
            .unwrap();
        assert_eq!(output, published, "startup-created repos must push commits");
    }
}

#[test]
fn repo_clone_skip_reuses_baked_checkout() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("cached.txt"), "preserve this checkout\n").unwrap();
    Command::new("git")
        .args(["add", "cached.txt"])
        .current_dir(repo.path())
        .success()
        .unwrap();
    create_commit(repo.path(), "Provide a file with a baked timestamp");
    write_test_devcontainer(
        &repo,
        &format!("RUN touch -d @946684800 {TEST_REPO_PATH}/cached.txt"),
        "",
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(
        repo.path().join(".rumpelpod.json"),
        merge_config(
            &executor.json,
            json!({"build": {"repoClone": {"mode": "skip"}}}),
        ),
    )
    .unwrap();
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "baked",
            "--",
            "stat",
            "-c",
            "%Y",
            "cached.txt",
        ])
        .success()
        .unwrap();
    assert_eq!(output, b"946684800\n");
}

#[test]
fn repo_clone_skip_preserves_files_without_a_baked_checkout() {
    let repo = TestRepo::new();
    write_test_devcontainer(
        &repo,
        &formatdoc! {"
            RUN rm -rf {TEST_REPO_PATH} && \\
                mkdir -p {TEST_REPO_PATH}/build-cache && \\
                echo cached > {TEST_REPO_PATH}/build-cache/artifact
        "},
        "",
    );
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(
        repo.path().join(".rumpelpod.json"),
        merge_config(
            &executor.json,
            json!({"build": {"repoClone": {"mode": "skip"}}}),
        ),
    )
    .unwrap();
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "cached",
            "--",
            "cat",
            "build-cache/artifact",
        ])
        .success()
        .unwrap();
    assert_eq!(output, b"cached\n");
}

#[test]
fn repo_clone_rejects_invalid_configuration() {
    let repo = TestRepo::new();
    for invalid in [
        "false",
        "true",
        "null",
        "\"skip\"",
        "{}",
        r#"{"mode":"remote","remote":"origin"}"#,
        r#"{"mode":"skpi"}"#,
        r#"{"mode":"skip","unexpected":true}"#,
    ] {
        fs::write(
            repo.path().join(".rumpelpod.json"),
            format!(r#"{{"build":{{"repoClone":{invalid}}}}}"#),
        )
        .unwrap();
        assert!(load_json_config(repo.path()).is_err(), "accepted {invalid}");
    }
    fs::write(
        repo.path().join(".rumpelpod.json"),
        indoc! {r#"{"build":{"repoClnoe":{"mode":"skip"}}}"#},
    )
    .unwrap();
    assert!(load_json_config(repo.path()).is_err());
}
