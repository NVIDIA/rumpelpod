// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Executor-agnostic test infrastructure.
//!
//! The executor mode is determined by environment variables:
//!   - (default):                     local Docker daemon
//!   - RUMPELPOD_TEST_EXECUTOR=podman or
//!     cargo xtest --executor podman: local Podman store
//!   - RUMPELPOD_TEST_EXECUTOR=ssh:   remote Docker via SSH
//!   - RUMPELPOD_EXECUTOR_CONFIG set: Kubernetes cluster
//!
//! Additional env vars control what gets copied into the test home:
//!   - KUBECONFIG:                    kubeconfig for k8s access
//!   - RUMPELPOD_TEST_DOCKER_CONFIG:  docker config.json (registry auth)
//!   - RUMPELPOD_TEST_BUILDX_CONFIG:  buildx config dir (builder instances)
//!
//! [`ExecutorResources`] sets up the active executor's resources and
//! returns the `.rumpelpod.json` body.  The daemon is started
//! separately by the test.

// Not all helpers are used by every executor.
#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Stdio};

use indoc::formatdoc;
use serde_json::{json, Value};

use super::common::TestHome;

/// Label applied to every sibling namespace created by the test
/// framework so that the xtest leftover check and end-of-run sweep
/// can find them.  Must stay in sync with the hardcoded selector
/// in `src/bin/xtest.rs::list_test_namespaces`.
pub const TEST_NAMESPACE_LABEL_KEY: &str = "rumpelhub/test-namespace";
pub const TEST_NAMESPACE_LABEL_VALUE: &str = "true";

/// Prefix for sibling test namespaces.  Chosen so `kubectl get ns`
/// output makes the origin obvious: anything starting with
/// `rumpelhub-test-` came from this framework.
pub const TEST_NAMESPACE_PREFIX: &str = "rumpelhub-test-";

// ---------------------------------------------------------------------------
// Executor mode
// ---------------------------------------------------------------------------

pub enum ExecutorMode {
    Docker,
    Podman,
    Ssh,
    K8s,
}

pub fn executor_mode() -> ExecutorMode {
    if std::env::var("RUMPELPOD_EXECUTOR_CONFIG").is_ok() {
        return ExecutorMode::K8s;
    }
    match std::env::var("RUMPELPOD_TEST_EXECUTOR").as_deref() {
        Ok("podman") => ExecutorMode::Podman,
        Ok("ssh") => ExecutorMode::Ssh,
        _ => ExecutorMode::Docker,
    }
}

/// Emit the xtest:skip directive so the test is reported as SKIPPED
/// rather than silently passing.  Call this before `return`ing from a
/// test that is not applicable for the current executor.
pub fn skip_test() {
    println!("xtest:skip");
}

/// Returns `true` when the current executor supports `rumpel stop`.
/// K8s does not.  Emits xtest:skip when returning false.
pub fn executor_supports_stop() -> bool {
    if matches!(executor_mode(), ExecutorMode::K8s) {
        skip_test();
        return false;
    }
    true
}

fn command_succeeds(command: &mut Command) -> bool {
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn podman_executor_available() -> bool {
    command_succeeds(Command::new("podman").arg("info"))
}

pub fn skip_unless_podman_executor() -> bool {
    if podman_executor_available() {
        true
    } else {
        skip_test();
        false
    }
}

// ---------------------------------------------------------------------------
// ExecutorResources -- executor-specific setup and .rumpelpod.json
// ---------------------------------------------------------------------------

/// Executor resources (SSH remote host) that must stay alive for the
/// duration of the test.
///
/// The `.rumpelpod.json` content is in [`json`]; tests write it to disk
/// themselves so the config is visible in the test body.
pub struct ExecutorResources {
    /// Executor-specific `.rumpelpod.json` content.
    pub json: String,
    /// Keep resources alive until the test ends.
    _resources: Resources,
}

enum Resources {
    Docker,
    Podman,
    Ssh { _remote: super::ssh::SshRemoteHost },
    K8s { namespace: K8sTestNamespace },
}

impl ExecutorResources {
    /// Set up the executor for the given test.
    ///
    /// For SSH mode, writes the SSH config into `home/.ssh/`.
    /// Links executor-specific host binaries into `home/.local/bin`
    /// so the daemon (which runs with `PATH=bin_dir`) can find them.
    /// Must be called before [`TestDaemon::start`] so the daemon
    /// inherits both the config and the bin dir via `$HOME`.
    pub fn setup(home: &TestHome) -> Self {
        match executor_mode() {
            ExecutorMode::Docker => Self::docker(home),
            ExecutorMode::Podman => Self::podman(home),
            ExecutorMode::Ssh => Self::ssh(home),
            ExecutorMode::K8s => Self::k8s(home),
        }
    }

    fn docker(home: &TestHome) -> Self {
        home.link_local_bin("docker");
        ExecutorResources {
            json: "{}".to_string(),
            _resources: Resources::Docker,
        }
    }

    pub fn podman(home: &TestHome) -> Self {
        if !podman_executor_available() {
            panic!("podman executor is not available");
        }
        home.link_local_bin("podman");
        let json = serde_json::to_string_pretty(&json!({
            "containerEngine": "podman",
        }))
        .unwrap();
        ExecutorResources {
            json,
            _resources: Resources::Podman,
        }
    }

    /// Create an SSH executor backed by a local Docker container
    /// running sshd.  Requires local Docker, so only usable in
    /// Docker executor mode.
    pub fn ssh(home: &TestHome) -> Self {
        // The daemon needs `docker` to manage the local infrastructure
        // container, and `ssh` to tunnel to it.
        home.link_local_bins(&["docker", "ssh"]);
        let remote = super::ssh::SshRemoteHost::start();
        super::ssh::write_ssh_config(home, &[&remote]);

        let remote_spec = remote.ssh_spec();
        let json = serde_json::to_string_pretty(&json!({
            "host": remote_spec,
        }))
        .unwrap();

        ExecutorResources {
            json,
            _resources: Resources::Ssh { _remote: remote },
        }
    }

    fn k8s(home: &TestHome) -> Self {
        let base_json = std::env::var("RUMPELPOD_EXECUTOR_CONFIG")
            .expect("RUMPELPOD_EXECUTOR_CONFIG must be set (use cargo xtest --executor <name>)");

        // Copy kubeconfig so the daemon can reach the cluster.
        let kubeconfig_src = std::env::var("KUBECONFIG")
            .expect("KUBECONFIG must be set (use cargo xtest --executor <name>)");
        let kubeconfig_dir = home.path().join(".kube");
        std::fs::create_dir_all(&kubeconfig_dir).unwrap();
        std::fs::copy(&kubeconfig_src, kubeconfig_dir.join("config"))
            .expect("failed to copy kubeconfig to test home");

        // `docker` is still needed to drive the buildx push into the
        // cluster registry, and `kubectl` for `rumpel enter` to exec
        // into pods.
        home.link_local_bins(&["docker", "kubectl"]);
        link_extra_path_bins(home);
        copy_test_docker_config(home);
        copy_test_buildx_config(home);

        let context = json_kubernetes_context(&base_json);
        let namespace = K8sTestNamespace::create(&context, Path::new(&kubeconfig_src));
        let json = substitute_namespace(&base_json, namespace.name());

        ExecutorResources {
            json,
            _resources: Resources::K8s { namespace },
        }
    }
}

/// Read the `kubernetes.context` field from a rumpelpod JSON config.
pub fn json_kubernetes_context(base_json: &str) -> String {
    let parsed: Value =
        json5::from_str(base_json).expect("failed to parse executor config for context");
    parsed
        .get("kubernetes")
        .and_then(|v| v.get("context"))
        .and_then(|v| v.as_str())
        .expect("executor config has no kubernetes.context")
        .to_string()
}

/// Substitute `"namespace": "<ns>"` into the `kubernetes` section of a
/// rumpelpod JSON config, preserving all other fields.
pub fn substitute_namespace(base_json: &str, namespace: &str) -> String {
    let mut parsed: Value = json5::from_str(base_json)
        .expect("failed to parse executor config for namespace substitution");
    let kubernetes = parsed
        .get_mut("kubernetes")
        .and_then(|v| v.as_object_mut())
        .expect("executor config has no kubernetes section");
    kubernetes.insert(
        "namespace".to_string(),
        Value::String(namespace.to_string()),
    );
    serde_json::to_string_pretty(&parsed).expect("failed to serialize patched executor config")
}

/// Merge extra fields into a base rumpelpod JSON config.  Tests use
/// this to extend the executor-provided config with their own
/// `merge` section without clobbering the executor's fields.
/// Top-level keys in `overlay` replace those in `base` outright --
/// there is no recursive merge, matching what tests need.
pub fn merge_config(base: &str, overlay: Value) -> String {
    let mut parsed: Value = json5::from_str(base).expect("failed to parse base config");
    let base_obj = parsed
        .as_object_mut()
        .expect("base config must be an object");
    let overlay_obj = overlay
        .as_object()
        .expect("overlay must be a JSON object at the top level");
    for (k, v) in overlay_obj {
        base_obj.insert(k.clone(), v.clone());
    }
    serde_json::to_string_pretty(&parsed).expect("failed to serialize merged config")
}

/// A per-test Kubernetes namespace.  Created with a unique name and
/// the `rumpelhub/test-namespace=true` label.  Drop fires a
/// best-effort non-blocking delete; the xtest end-of-run sweep
/// guarantees cleanup of anything Drop misses.
pub struct K8sTestNamespace {
    context: String,
    kubeconfig: std::path::PathBuf,
    name: String,
}

impl K8sTestNamespace {
    pub fn create(context: &str, kubeconfig: &Path) -> Self {
        use rand::distr::Alphanumeric;
        use rand::RngExt;

        // Random alphanumeric suffix so concurrent tests in the same
        // cluster do not collide, and so a kubectl operator can still
        // eyeball which namespace is whose.
        let suffix: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(|b| (b as char).to_ascii_lowercase())
            .collect();
        let name = format!("{TEST_NAMESPACE_PREFIX}{suffix}");

        let manifest = formatdoc! {r#"
            apiVersion: v1
            kind: Namespace
            metadata:
              name: {name}
              labels:
                {TEST_NAMESPACE_LABEL_KEY}: "{TEST_NAMESPACE_LABEL_VALUE}"
        "#};

        let mut child = Command::new("kubectl")
            .args(["--kubeconfig", &kubeconfig.display().to_string()])
            .args(["--context", context])
            .args(["apply", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn kubectl apply");
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), manifest.as_bytes())
            .expect("failed to write namespace manifest to kubectl");
        drop(child.stdin.take());
        let output = child.wait_with_output().expect("kubectl apply failed");
        assert!(
            output.status.success(),
            "failed to create test namespace {name}: {}",
            String::from_utf8_lossy(&output.stderr),
        );

        Self {
            context: context.to_string(),
            kubeconfig: kubeconfig.to_path_buf(),
            name,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for K8sTestNamespace {
    fn drop(&mut self) {
        let _ = Command::new("kubectl")
            .args(["--kubeconfig", &self.kubeconfig.display().to_string()])
            .args(["--context", &self.context])
            .args(["delete", "namespace", &self.name, "--wait=false"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Symlink any binaries from `RUMPELPOD_EXTRA_PATH` into the test bin
/// dir.  `xtest.rs` sets this when running against EKS so the daemon
/// can find `docker-credential-ecr-login` for ECR pushes.  Silently
/// ignored when the env var is unset.
pub fn link_extra_path_bins(home: &TestHome) {
    let Ok(extra) = std::env::var("RUMPELPOD_EXTRA_PATH") else {
        return;
    };
    for dir in extra.split(':') {
        if dir.is_empty() {
            continue;
        }
        home.link_local_bins_from_dir(Path::new(dir));
    }
}

/// If `RUMPELPOD_TEST_DOCKER_CONFIG` is set, copy it to the test home
/// as `~/.docker/config.json`.
pub fn copy_test_docker_config(home: &TestHome) {
    if let Ok(src) = std::env::var("RUMPELPOD_TEST_DOCKER_CONFIG") {
        let src = Path::new(&src);
        if src.exists() {
            let dst_dir = home.path().join(".docker");
            std::fs::create_dir_all(&dst_dir).unwrap();
            std::fs::copy(src, dst_dir.join("config.json"))
                .expect("failed to copy docker config.json from RUMPELPOD_TEST_DOCKER_CONFIG");
        } else {
            let path = src.display();
            panic!("RUMPELPOD_TEST_DOCKER_CONFIG points to non-existent file: {path}");
        }
    }
}

/// If `RUMPELPOD_TEST_BUILDX_CONFIG` is set, copy its `instances/`
/// entries to the test home at `~/.docker/buildx/instances/`.
pub fn copy_test_buildx_config(home: &TestHome) {
    if let Ok(src_dir) = std::env::var("RUMPELPOD_TEST_BUILDX_CONFIG") {
        let src_instances = Path::new(&src_dir).join("instances");
        if src_instances.exists() {
            let dst_buildx = home.path().join(".docker/buildx/instances");
            std::fs::create_dir_all(&dst_buildx).unwrap();
            for entry in std::fs::read_dir(&src_instances).unwrap().flatten() {
                std::fs::copy(entry.path(), dst_buildx.join(entry.file_name()))
                    .expect("failed to copy buildx instance config");
            }
        }
    }
}
