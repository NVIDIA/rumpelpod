// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root above rumpelpod crate")
        .to_path_buf()
}

#[test]
fn pipeline_denies_warnings_without_overriding_target_rustflags() {
    let root = workspace_root();
    let workspace_manifest =
        fs::read_to_string(root.join("Cargo.toml")).expect("read workspace manifest");
    assert!(
        workspace_manifest.contains("[workspace.lints.rust]")
            && workspace_manifest.contains("warnings = \"deny\""),
        "the workspace did not deny Rust warnings"
    );
    for member in ["rumpelpod", "tools"] {
        let member_manifest = fs::read_to_string(root.join(member).join("Cargo.toml"))
            .unwrap_or_else(|error| panic!("read {member} manifest: {error}"));
        assert!(
            member_manifest.contains("[lints]\nworkspace = true"),
            "{member} did not inherit workspace lints"
        );
    }

    let pipeline =
        fs::read_to_string(root.join("tools/src/bin/pipeline.rs")).expect("read Cargo pipeline");
    assert!(
        pipeline.contains("command.env_remove(\"RUSTFLAGS\")")
            && !pipeline.contains(".env(\"RUSTFLAGS\""),
        "pipeline builds did not preserve target-specific Rust flags"
    );
    assert_eq!(
        pipeline.matches("tools::cargo_cmd()").count(),
        1,
        "a pipeline Cargo command bypassed the target-flag-preserving wrapper"
    );
}

#[test]
fn devcontainer_installs_the_built_daemon_from_pipeline() {
    let root = workspace_root();
    let config: serde_json::Value = json5::from_str(
        &fs::read_to_string(root.join(".devcontainer/devcontainer.json"))
            .expect("read development container configuration"),
    )
    .expect("parse development container configuration");
    assert!(
        config.get("postCreateCommand").is_none(),
        "daemon installation depended on a post-create lifecycle command"
    );

    let dockerfile =
        fs::read_to_string(root.join(".devcontainer/Dockerfile")).expect("read Dockerfile");
    assert!(
        dockerfile.contains("touch /var/lib/systemd/linger/${USER}"),
        "the image did not arrange a user manager at container boot"
    );
    assert!(
        !dockerfile.contains("system-install")
            && !dockerfile.contains("rumpelpod.socket")
            && !dockerfile.contains("rumpelpod.service"),
        "the image came with daemon units built from an unrelated revision"
    );
    assert!(
        !dockerfile.contains("code-server")
            && !dockerfile.contains("chromium")
            && !dockerfile.contains("rumpelpod-vscode"),
        "the daemon devcontainer included extension-specific services"
    );
    assert!(
        dockerfile.contains("ENV RUMPELPOD_DEVCONTAINER=1")
            && dockerfile.contains("ENV XDG_RUNTIME_DIR=/run/user/${USER_ID}")
            && dockerfile
                .contains("ENV DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/${USER_ID}/bus"),
        "the image did not expose the deferred daemon installation environment"
    );

    let pipeline =
        fs::read_to_string(root.join("tools/src/bin/pipeline.rs")).expect("read Cargo pipeline");
    let build = pipeline
        .find(".args([\"build\", \"--all-targets\"])")
        .expect("pipeline did not build workspace binaries");
    let install = pipeline
        .find("install_devcontainer_daemon(release)?")
        .expect("pipeline did not install the development daemon");
    assert!(
        build < install
            && pipeline.contains("const DEVCONTAINER_ENV: &str = \"RUMPELPOD_DEVCONTAINER\"")
            && pipeline.contains("rumpel-linux-amd64")
            && pipeline.contains("rumpel-linux-arm64")
            && pipeline.contains("--user\", \"show-environment")
            && pipeline.contains(".arg(\"system-install\")")
            && !pipeline.contains("cargo vscode")
            && !pipeline.contains("[\"vscode\""),
        "the marked pipeline did not install its freshly built daemon and payloads"
    );

    let ci_pipeline =
        fs::read_to_string(root.join("ci/run-pipeline.sh")).expect("read CI pipeline script");
    assert!(
        ci_pipeline.contains("manager_ready=0")
            && ci_pipeline.contains("docker inspect --format '{{.State.Running}}'")
            && ci_pipeline.contains("docker logs devcontainer"),
        "CI did not report a failed devcontainer boot"
    );
}

#[test]
fn devcontainer_keeps_pinned_codex_out_of_the_interactive_path() {
    let root = workspace_root();
    let dockerfile =
        fs::read_to_string(root.join(".devcontainer/Dockerfile")).expect("read Dockerfile");
    assert!(
        dockerfile.contains("ARG CODEX_VERSION=0.154.0")
            && dockerfile
                .contains("ENV RUMPELPOD_TEST_CODEX_BIN_DIR=/opt/rumpelpod-test-codex/bin")
            && !dockerfile.contains("/usr/local/bin/codex"),
        "the development image did not isolate its pinned Codex test package"
    );

    let pipeline =
        fs::read_to_string(root.join("tools/src/bin/pipeline.rs")).expect("read Cargo pipeline");
    assert!(
        pipeline.contains("const TEST_CODEX_BIN_DIR_ENV: &str = \"RUMPELPOD_TEST_CODEX_BIN_DIR\"")
            && pipeline.contains("use_pinned_test_codex(&mut xtest_cmd)?"),
        "the test pipeline did not select the isolated Codex package"
    );
}

#[test]
fn xtest_timeouts_file_is_the_only_non_default_timeout_source() {
    let root = workspace_root();
    let path = root.join("tools/xtest-timeouts.json5");
    let contents = fs::read_to_string(&path).expect("read xtest timeouts file");
    let parsed: serde_json::Map<String, serde_json::Value> =
        json5::from_str(&contents).expect("parse xtest timeouts file");
    assert!(!parsed.is_empty(), "xtest timeouts file is empty");
    for (name, value) in &parsed {
        let Some(multiplier) = value.as_u64() else {
            panic!("{name} is not an integer multiplier");
        };
        assert!(
            multiplier >= 2,
            "{name} multiplier {multiplier} must be omitted if it is the default"
        );
    }

    for dir in ["rumpelpod/tests", "tools/src/bin"] {
        for entry in rust_files_under(root.join(dir)) {
            let path = entry.display();
            let text =
                fs::read_to_string(&entry).unwrap_or_else(|error| panic!("read {path}: {error}"));
            assert!(
                !text.contains("println!(\"xtest:timeout="),
                "{path} still prints an xtest timeout"
            );
        }
    }
}

#[test]
fn xtest_reports_an_immediate_skip() {
    println!("xtest:skip");
}

fn rust_files_under(dir: PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir];
    while let Some(dir) = stack.pop() {
        let dir_display = dir.display();
        for entry in
            fs::read_dir(&dir).unwrap_or_else(|error| panic!("read {dir_display}: {error}"))
        {
            let entry = entry.unwrap_or_else(|error| panic!("dir entry: {error}"));
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}
