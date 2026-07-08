// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for devcontainer build paths outside the repository.

use std::fs;
use std::process::Command;

use indoc::formatdoc;

use crate::common::{pod_command, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

fn write_devcontainer(repo: &TestRepo, dockerfile: &str, context: &str) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    fs::create_dir_all(&devcontainer_dir).expect("create .devcontainer directory");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "{dockerfile}",
                "context": "{context}"
            }}
        }}
    "#};
    fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("write devcontainer.json");
}

#[test]
fn image_build_errors_if_dockerfile_is_outside_repo() {
    let repo = TestRepo::new();
    let outside_dockerfile = repo
        .path()
        .parent()
        .expect("test repo has a parent")
        .join("Dockerfile");
    write_devcontainer(&repo, &outside_dockerfile.display().to_string(), "..");

    let output = Command::new("rumpel")
        .args(["image", "build"])
        .current_dir(repo.path())
        .output()
        .expect("run rumpel image build");

    assert!(!output.status.success(), "image build should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("devcontainer dockerfile path must stay under the repo root"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn enter_errors_if_build_context_is_outside_repo() {
    let repo = TestRepo::new();
    let outside_context = repo.path().parent().expect("test repo has a parent");
    write_devcontainer(&repo, "Dockerfile", &outside_context.display().to_string());

    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).expect("write .rumpelpod.json");

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "outside-context", "--", "true"])
        .output()
        .expect("run rumpel enter");

    assert!(!output.status.success(), "enter should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("devcontainer build context path must stay under the repo root"),
        "unexpected stderr: {stderr}"
    );
}
