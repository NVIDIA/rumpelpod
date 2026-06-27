// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for Kubernetes host support.
//!
//! In Docker executor mode (the default), tests use the ambient k3d
//! cluster configured in cloud/k3d/.  With --executor, tests run
//! against the provided cluster instead.  Tests skip when no k8s
//! config is available (e.g. SSH mode, or Docker mode without k3d).

mod hub;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use indoc::formatdoc;
use retry::delay::Fixed;
use retry::OperationResult;
use rumpelpod::CommandExt;

use crate::common::{
    pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo, TEST_REPO_PATH,
};
use crate::executor::{
    executor_mode, json_kubernetes_context, skip_test, substitute_namespace, ExecutorMode,
    K8sTestNamespace,
};

const K3D_DIR: &str = "cloud/k3d";

struct EnvVarGuard {
    key: &'static str,
    old: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: PathBuf) -> Self {
        let old = env::var_os(key);
        env::set_var(key, value);
        Self { key, old }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(value) => env::set_var(self.key, value),
            None => env::remove_var(self.key),
        }
    }
}

/// Check whether a k8s executor is available.  Emits xtest:skip when
/// returning false so the test shows as SKIPPED.
fn has_k8s_executor() -> bool {
    match executor_mode() {
        ExecutorMode::K8s => true,
        ExecutorMode::Docker | ExecutorMode::Podman => {
            if std::path::Path::new(K3D_DIR)
                .join("rumpelpod.json")
                .exists()
            {
                true
            } else {
                skip_test();
                false
            }
        }
        ExecutorMode::Ssh => {
            skip_test();
            false
        }
    }
}

/// Read the base executor config (context + registry, no namespace).
///
/// In K8s executor mode, reads from env vars set by --executor.  In
/// Docker mode, falls back to the ambient cloud/k3d/ directory.
fn k8s_base_config() -> String {
    match executor_mode() {
        ExecutorMode::K8s => std::env::var("RUMPELPOD_EXECUTOR_CONFIG")
            .expect("RUMPELPOD_EXECUTOR_CONFIG must be set in K8s executor mode"),
        ExecutorMode::Docker | ExecutorMode::Podman | ExecutorMode::Ssh => {
            let path = std::path::Path::new(K3D_DIR).join("rumpelpod.json");
            std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        }
    }
}

fn k8s_kubeconfig() -> PathBuf {
    match executor_mode() {
        ExecutorMode::K8s => PathBuf::from(
            std::env::var("KUBECONFIG").expect("KUBECONFIG must be set in K8s executor mode"),
        ),
        ExecutorMode::Docker | ExecutorMode::Podman | ExecutorMode::Ssh => {
            std::path::Path::new(K3D_DIR).join("kubeconfig")
        }
    }
}

/// Set up k8s executor resources for a test: copy kubeconfig, registry
/// auth, and buildx config to the test home, and create a sibling
/// namespace owned by this test that is deleted on drop.
fn k8s_executor(home: &TestHome) -> K8sExecutor {
    let base_config = k8s_base_config();
    let context = json_kubernetes_context(&base_config);
    let kubeconfig_src = k8s_kubeconfig();

    let kubeconfig_dst = home.path().join(".kube");
    std::fs::create_dir_all(&kubeconfig_dst).unwrap();
    let kubeconfig_file = kubeconfig_dst.join("config");
    std::fs::copy(&kubeconfig_src, &kubeconfig_file)
        .expect("failed to copy kubeconfig to test home");

    // Point KUBECONFIG at the copy so the daemon subprocess finds it
    // regardless of how its HOME is resolved.
    std::env::set_var("KUBECONFIG", &kubeconfig_file);

    // The daemon runs with PATH=home/.local/bin, so it needs its
    // Kubernetes and image-builder CLIs symlinked before it starts.
    home.link_local_bins(&["docker", "kubectl"]);
    crate::executor::link_extra_path_bins(home);
    crate::executor::copy_test_docker_config(home);
    crate::executor::copy_test_buildx_config(home);

    let namespace_guard = K8sTestNamespace::create(&context, &kubeconfig_src);
    let namespace = namespace_guard.name().to_string();
    let json = substitute_namespace(&base_config, &namespace);

    K8sExecutor {
        json,
        context,
        namespace,
        _namespace_guard: namespace_guard,
    }
}

struct K8sExecutor {
    json: String,
    context: String,
    namespace: String,
    /// Dropping this fires a non-blocking namespace delete.
    _namespace_guard: K8sTestNamespace,
}

#[test]
fn k8s_unavailable_reentry_preserves_pod_record() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let pod_name = "k8s-down";
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let mut daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "true"])
        .success()
        .expect("rumpel enter failed");

    daemon.kill();
    drop(daemon);

    let _kubeconfig = EnvVarGuard::set("KUBECONFIG", home.path().join("missing-kubeconfig"));
    let daemon = TestDaemon::start(&home);
    let output = pod_command(&repo, &daemon)
        .args(["enter", pod_name, "--", "true"])
        .output()
        .expect("rumpel enter failed to execute");

    assert!(
        !output.status.success(),
        "enter should fail while the k8s connection is unavailable"
    );

    let stdout = pod_command(&repo, &daemon)
        .arg("list")
        .success()
        .expect("rumpel list failed");
    let list_output = String::from_utf8_lossy(&stdout);
    assert!(
        list_output.contains(pod_name),
        "list should still show pod after reconnect failure: {list_output}",
    );
    assert!(
        list_output.contains("disconnected"),
        "list should report disconnected status after reconnect failure: {list_output}",
    );
}

#[test]
fn k8s_enter_smoke() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-test",
            "--",
            "echo",
            "hello from k8s",
        ])
        .output()
        .expect("rumpel enter failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "rumpel enter failed: stdout={}, stderr={}",
        stdout,
        stderr,
    );
    assert_eq!(stdout.trim(), "hello from k8s");
}

#[test]
fn k8s_list_shows_pod() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod first
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "k8s-list-test", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    // Check it shows up in list
    let stdout = pod_command(&repo, &daemon)
        .args(["list"])
        .success()
        .expect("rumpel list failed");

    let output = String::from_utf8_lossy(&stdout);
    assert!(
        output.contains("k8s-list-test"),
        "list should show pod: {}",
        output,
    );
}

/// K8s mirror of `list::list_shows_claude_state`: after a pod reports
/// claude state via the in-pod hook, `rumpel list` should show the
/// CLAUDE column with the human-readable state.
#[test]
fn k8s_list_shows_claude_state() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "claude-st", "--", "echo", "hello"])
        .success()
        .expect("rumpel enter failed");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "claude-st",
            "--",
            "/opt/rumpelpod/bin/rumpel",
            "claude-hook",
            "notify-state",
            "processing",
        ])
        .success()
        .expect("claude-hook notify-state failed");

    let stdout = retry::retry(Fixed::from_millis(500).take(120), || {
        let output = pod_command(&repo, &daemon)
            .arg("list")
            .output()
            .expect("rumpel list failed");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.contains("CLAUDE") {
            OperationResult::Ok(stdout)
        } else {
            OperationResult::Retry(stdout)
        }
    })
    .expect("CLAUDE column never appeared in list output");

    assert!(
        stdout.contains("processing"),
        "expected 'processing' in CLAUDE column: {stdout}",
    );
}

#[test]
fn k8s_delete_removes_pod() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let context = &executor.context;
    let namespace = &executor.namespace;

    // Create a pod
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "k8s-del-test", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    // Delete it
    pod_command(&repo, &daemon)
        .args(["delete", "--force", "k8s-del-test"])
        .success()
        .expect("rumpel delete failed");

    // Verify it's gone from list
    let stdout = pod_command(&repo, &daemon)
        .args(["list"])
        .success()
        .expect("rumpel list failed");

    let output = String::from_utf8_lossy(&stdout);
    assert!(
        !output.contains("k8s-del-test"),
        "deleted pod should not appear in list: {}",
        output,
    );

    // Verify k8s pod is also gone
    let kubectl_output = Command::new("kubectl")
        .args(["--context", context])
        .args(["--namespace", namespace])
        .args(["get", "pod", "-l", "rumpelpod/pod-name=k8s-del-test"])
        .args(["-o", "name"])
        .output()
        .expect("kubectl get failed");
    let pods = String::from_utf8_lossy(&kubectl_output.stdout);
    assert!(
        pods.trim().is_empty(),
        "k8s pod should be deleted: {}",
        pods,
    );
}

/// If the underlying k8s pod is already gone (e.g. the user deleted it
/// out-of-band via kubectl), `rumpel delete` should still clean up the
/// daemon-side state instead of erroring out with NotFound.
#[test]
fn k8s_delete_succeeds_when_pod_already_gone() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let context = &executor.context;
    let namespace = &executor.namespace;

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "k8s-del-gone", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    // Delete the underlying k8s pod out-of-band so the daemon still
    // has a DB record but the API call will return NotFound.
    Command::new("kubectl")
        .args(["--context", context])
        .args(["--namespace", namespace])
        .args(["delete", "pod", "-l", "rumpelpod/pod-name=k8s-del-gone"])
        .args(["--wait=true", "--grace-period=0", "--force"])
        .success()
        .expect("kubectl delete failed");

    pod_command(&repo, &daemon)
        .args(["delete", "--force", "k8s-del-gone"])
        .success()
        .expect("rumpel delete should succeed when k8s pod is already gone");

    let stdout = pod_command(&repo, &daemon)
        .args(["list"])
        .success()
        .expect("rumpel list failed");
    let output = String::from_utf8_lossy(&stdout);
    assert!(
        !output.contains("k8s-del-gone"),
        "deleted pod should not appear in list: {}",
        output,
    );
}

#[test]
fn k8s_image_build_no_registry() {
    if !has_k8s_executor() {
        return;
    }
    // A `kubernetes` section without `registry` is a configuration error:
    // every Kubernetes launch builds a prepared image that the cluster
    // must pull, so the registry is required up-front rather than failing
    // later at image-build time.
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let context = &executor.context;
    let namespace = &executor.namespace;

    let config = serde_json::to_string(&serde_json::json!({
        "kubernetes": {
            "context": context,
            "namespace": namespace,
        }
    }))
    .unwrap();
    fs::write(repo.path().join(".rumpelpod.json"), config)
        .expect("Failed to write .rumpelpod.json");

    let daemon = TestDaemon::start(&home);

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "build-test", "--", "true"])
        .output()
        .expect("rumpel enter failed to execute");

    assert!(
        !output.status.success(),
        "should fail when 'kubernetes' is configured without a registry",
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("registry"),
        "error should mention the missing registry field: {}",
        stderr,
    );
}

#[test]
fn k8s_image_build() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-build-test",
            "--",
            "echo",
            "hello from built image",
        ])
        .output()
        .expect("rumpel enter failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "rumpel enter with built image failed: stdout={}, stderr={}",
        stdout,
        stderr,
    );
    // stdout includes build/push output before the command output
    assert!(
        stdout.trim().ends_with("hello from built image"),
        "command output should end with expected message: {}",
        stdout,
    );
}

#[test]
fn k8s_cp_to_pod() {
    println!("xtest:timeout=300");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "k8s-cp-to", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    let local_file = repo.path().join("test-upload.txt");
    fs::write(&local_file, "hello from host").expect("Failed to write local file");

    pod_command(&repo, &daemon)
        .args([
            "cp",
            &local_file.to_string_lossy(),
            "k8s-cp-to:/tmp/test-upload.txt",
        ])
        .success()
        .expect("rumpel cp to pod failed");

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-cp-to",
            "--",
            "cat",
            "/tmp/test-upload.txt",
        ])
        .success()
        .expect("reading file in pod failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hello from host");
}

#[test]
fn k8s_cp_from_pod() {
    println!("xtest:timeout=300");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-cp-from",
            "--",
            "sh",
            "-c",
            "echo 'hello from pod' > /tmp/test.txt",
        ])
        .success()
        .expect("rumpel enter failed");

    let local_download = repo.path().join("test-download.txt");
    pod_command(&repo, &daemon)
        .args([
            "cp",
            "k8s-cp-from:/tmp/test.txt",
            &local_download.to_string_lossy(),
        ])
        .success()
        .expect("rumpel cp from pod failed");

    let content = fs::read_to_string(&local_download).expect("Failed to read downloaded file");
    assert_eq!(content.trim(), "hello from pod");
}

#[test]
fn k8s_mount_volume() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(
        &repo,
        "",
        r#","mounts": [{"type":"volume","source":"tv","target":"/data"}]"#,
    );
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Write a file to the volume mount
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-vol-test",
            "--",
            "sh",
            "-c",
            "echo hello > /data/test.txt",
        ])
        .success()
        .expect("writing to volume failed");

    // Read it back
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-vol-test",
            "--",
            "cat",
            "/data/test.txt",
        ])
        .success()
        .expect("reading from volume failed");

    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hello");
}

#[test]
fn k8s_mount_tmpfs() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(
        &repo,
        "",
        r#","mounts": [{"type":"tmpfs","target":"/tmp/mytmp"}]"#,
    );
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Check that the tmpfs mount is present via /proc/mounts
    // (the `mount` binary is not available in wolfi-base)
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-tmpfs-test",
            "--",
            "cat",
            "/proc/mounts",
        ])
        .success()
        .expect("reading /proc/mounts failed");

    let output = String::from_utf8_lossy(&stdout);
    // emptyDir with Memory medium shows up as tmpfs
    assert!(
        output.contains("/tmp/mytmp") && output.contains("tmpfs"),
        "expected tmpfs at /tmp/mytmp in /proc/mounts: {}",
        output,
    );
}

#[test]
fn k8s_bind_mount_uploaded() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);

    // On k8s, bind mounts are converted to emptyDir volumes and
    // populated via tar upload from the host.
    let bind_src = repo.path().join("bind-data");
    fs::create_dir_all(&bind_src).unwrap();
    fs::write(bind_src.join("hello.txt"), "from-host").unwrap();

    let bind_src_str = bind_src.display();
    let mount_json =
        format!(r#","mounts": [{{"type":"bind","source":"{bind_src_str}","target":"/mnt/data"}}]"#);
    write_test_devcontainer(&repo, "", &mount_json);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-bind-test",
            "--",
            "cat",
            "/mnt/data/hello.txt",
        ])
        .success()
        .expect("rumpel enter with bind mount failed");

    assert_eq!(
        String::from_utf8_lossy(&stdout).trim(),
        "from-host",
        "bind mount content should be uploaded to k8s pod",
    );
}

#[test]
fn k8s_privileged() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", r#","privileged": true"#);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-priv-test",
            "--",
            "echo",
            "privileged-ok",
        ])
        .success()
        .expect("rumpel enter with privileged failed");

    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "privileged-ok");
}

#[test]
fn k8s_cap_add() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", r#","capAdd": ["SYS_PTRACE"]"#);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "--create", "k8s-cap-test", "--", "echo", "caps-ok"])
        .success()
        .expect("rumpel enter with capAdd failed");

    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "caps-ok");
}

#[test]
fn k8s_override_command_false() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(
        &repo,
        r#"CMD ["tail", "-f", "/dev/null"]"#,
        r#","overrideCommand": false"#,
    );
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Check PID 1 is tail, not sleep
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-nocmd-test",
            "--",
            "cat",
            "/proc/1/cmdline",
        ])
        .success()
        .expect("checking PID 1 failed");

    let cmdline = String::from_utf8_lossy(&stdout);
    assert!(
        cmdline.contains("tail"),
        "PID 1 should be tail, got: {:?}",
        cmdline,
    );
}

/// overrideCommand (the default) must override the CMD, not the
/// ENTRYPOINT.  In k8s terms, it should set `args` (CMD) not
/// `command` (ENTRYPOINT).
#[test]
fn k8s_entrypoint_preserved_with_override_command() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    // ENTRYPOINT writes a marker then execs CMD; overrideCommand
    // replaces CMD with sleep infinity but the entrypoint must still run.
    write_test_devcontainer(
        &repo,
        &formatdoc! {r#"
            RUN printf '#!/bin/sh\ntouch /tmp/entrypoint-ran\nexec "$@"\n' > /entrypoint.sh \
                && chmod +x /entrypoint.sh
            ENTRYPOINT ["/entrypoint.sh"]
            CMD ["sleep", "infinity"]
        "#},
        "",
    );
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // The entrypoint touches a marker file and then exec's the CMD.
    // If the bug is present (k8s `command` set instead of `args`),
    // the entrypoint is skipped entirely and the marker is absent.
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-ep-test",
            "--",
            "test",
            "-f",
            "/tmp/entrypoint-ran",
        ])
        .success()
        .expect("entrypoint marker should exist -- entrypoint was skipped");
}

#[test]
fn k8s_host_requirements() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(
        &repo,
        "",
        r#","hostRequirements": {"cpus": 1, "memory": "256mb"}"#,
    );
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let context = &executor.context;
    let namespace = &executor.namespace;

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "k8s-reqs-test", "--", "true"])
        .success()
        .expect("rumpel enter with hostRequirements failed");

    // Verify resource requests are set via kubectl
    let kubectl_output = Command::new("kubectl")
        .args(["--context", context])
        .args(["--namespace", namespace])
        .args([
            "get",
            "pod",
            "-l",
            "rumpelpod/pod-name=k8s-reqs-test",
            "-o",
            "jsonpath={.items[0].spec.containers[0].resources.requests}",
        ])
        .output()
        .expect("kubectl get failed");

    let requests = String::from_utf8_lossy(&kubectl_output.stdout);
    assert!(
        requests.contains("256Mi"),
        "resource requests should include memory: {}",
        requests,
    );
    assert!(
        requests.contains("1"),
        "resource requests should include cpu: {}",
        requests,
    );
}

#[test]
fn k8s_init_succeeds_despite_unsupported() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", r#","init": true"#);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // init: true is unsupported on k8s but should not prevent pod creation
    // (the daemon logs a warning instead of failing)
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-init-test",
            "--",
            "echo",
            "init-ok",
        ])
        .success()
        .expect("init: true should not prevent pod creation on k8s");
}

#[test]
fn k8s_runtime_class_name() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);

    // Create a RuntimeClass backed by the runc handler (always available).
    let manifest = indoc::indoc! {"
        apiVersion: node.k8s.io/v1
        kind: RuntimeClass
        metadata:
          name: test-runc
        handler: runc
    "};
    let mut child = Command::new("kubectl")
        .args(["--context", &executor.context])
        .args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn kubectl apply");
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), manifest.as_bytes())
        .expect("failed to write RuntimeClass manifest");
    drop(child.stdin.take());
    let status = child.wait().expect("kubectl apply failed");
    assert!(status.success(), "failed to create RuntimeClass test-runc");

    write_test_devcontainer(&repo, "", r#","runArgs": ["--runtime=test-runc"]"#);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-rt-test",
            "--",
            "echo",
            "runtime-ok",
        ])
        .success()
        .expect("rumpel enter with --runtime=test-runc failed");

    // Verify the pod has runtimeClassName set
    let kubectl_output = Command::new("kubectl")
        .args(["--context", &executor.context])
        .args(["--namespace", &executor.namespace])
        .args([
            "get",
            "pod",
            "-l",
            "rumpelpod/pod-name=k8s-rt-test",
            "-o",
            "jsonpath={.items[0].spec.runtimeClassName}",
        ])
        .output()
        .expect("kubectl get failed");

    let runtime_class = String::from_utf8_lossy(&kubectl_output.stdout);
    assert_eq!(
        runtime_class.trim(),
        "test-runc",
        "pod should have runtimeClassName=test-runc, got: {runtime_class}",
    );
}

#[test]
fn k8s_runtime_runc_omitted() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);

    // --runtime=runc is the Docker default; on k8s it should be
    // omitted from the pod spec rather than setting runtimeClassName.
    write_test_devcontainer(&repo, "", r#","runArgs": ["--runtime=runc"]"#);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-runc-test",
            "--",
            "echo",
            "runc-ok",
        ])
        .success()
        .expect("rumpel enter with --runtime=runc should succeed on k8s");

    // Verify the pod does NOT have runtimeClassName set
    let kubectl_output = Command::new("kubectl")
        .args(["--context", &executor.context])
        .args(["--namespace", &executor.namespace])
        .args([
            "get",
            "pod",
            "-l",
            "rumpelpod/pod-name=k8s-runc-test",
            "-o",
            "jsonpath={.items[0].spec.runtimeClassName}",
        ])
        .output()
        .expect("kubectl get failed");

    let runtime_class = String::from_utf8_lossy(&kubectl_output.stdout);
    assert!(
        runtime_class.trim().is_empty(),
        "pod should not have runtimeClassName when --runtime=runc, got: {runtime_class}",
    );
}

// Port forwarding to user-specified ports (forwardPorts) relies on
// kubectl port-forward, which does not work reliably through k3d's
// Docker-in-Docker networking.  Keep under #[ignore] until we have
// a remote cluster in CI.
#[test]
#[ignore]
fn k8s_forward_port() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    // Install socat alongside git for the echo server
    write_test_devcontainer(
        &repo,
        "RUN apk add --no-cache socat",
        r#","forwardPorts": [9600]"#,
    );
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create the pod (this should set up the port forward)
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "k8s-fwd-test", "--", "true"])
        .success()
        .expect("rumpel enter failed");

    // Start a socat listener on port 9600 inside the pod
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-fwd-test",
            "--",
            "sh",
            "-c",
            "nohup socat TCP-LISTEN:9600,fork,reuseaddr EXEC:cat >/dev/null 2>&1 &",
        ])
        .success()
        .expect("Failed to start echo server");

    std::thread::sleep(Duration::from_millis(500));

    // Check rumpel ports shows the forwarded port
    let stdout = pod_command(&repo, &daemon)
        .args(["ports", "k8s-fwd-test"])
        .success()
        .expect("rumpel ports failed");

    let ports_output = String::from_utf8_lossy(&stdout);
    assert!(
        ports_output.contains("9600"),
        "ports output should mention port 9600: {}",
        ports_output,
    );

    // Extract the local port from the ports output.
    // Format: "CONTAINER    LOCAL    LABEL"
    //         "9600         43567"
    let local_port: u16 = ports_output
        .lines()
        .find(|l| l.contains("9600") && !l.contains("CONTAINER"))
        .and_then(|l| l.split_whitespace().nth(1).and_then(|s| s.parse().ok()))
        .expect("could not parse local port from ports output");

    // Verify TCP connection through the forwarded port succeeds.
    // kubectl port-forward and the in-pod socat may need a moment
    // to become ready, especially on k3d.
    let mut last_err = None;
    for _ in 0..20 {
        match TcpStream::connect(format!("127.0.0.1:{local_port}")) {
            Ok(stream) => {
                drop(stream);
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    if let Some(e) = last_err {
        panic!("TCP connect to forwarded port {local_port} failed after retries: {e}");
    }
}

#[test]
fn k8s_recreate() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Create a pod and write a dirty (uncommitted) file
    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-recreate-test",
            "--",
            "sh",
            "-c",
            "echo dirty-content > /home/testuser/workspace/dirty.txt",
        ])
        .success()
        .expect("writing dirty file failed");

    // Recreate the pod
    pod_command(&repo, &daemon)
        .args(["recreate", "k8s-recreate-test"])
        .success()
        .expect("rumpel recreate failed");

    // Verify the dirty file survived the recreate
    let stdout = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            "k8s-recreate-test",
            "--",
            "cat",
            "/home/testuser/workspace/dirty.txt",
        ])
        .success()
        .expect("reading dirty file after recreate failed");

    assert_eq!(
        String::from_utf8_lossy(&stdout).trim(),
        "dirty-content",
        "dirty file should survive recreate",
    );
}

#[test]
fn k8s_fork_smoke() {
    println!("xtest:timeout=300");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
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
            "echo source > marker.txt",
        ])
        .success()
        .expect("rumpel enter src failed");

    pod_command(&repo, &daemon)
        .args(["fork", "src", "fk"])
        .success()
        .expect("rumpel fork failed");

    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "fk", "--", "cat", "marker.txt"])
        .success()
        .expect("read marker on fork failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "source");
}

#[test]
fn k8s_node_selector_and_tolerations() {
    println!("xtest:timeout=240");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");

    let context = &executor.context;
    let namespace = &executor.namespace;

    // Merge extra node-selector and toleration into the executor's config.
    // Cannot just replace the kubernetes section because some executors
    // (e.g. EKS) already populate node-selector or tolerations there.
    let mut config: serde_json::Value =
        json5::from_str(&executor.json).expect("failed to parse executor config");
    let kubernetes = config
        .get_mut("kubernetes")
        .and_then(|v| v.as_object_mut())
        .expect("executor config has no kubernetes section");
    let ns = kubernetes
        .entry("nodeSelector".to_string())
        .or_insert_with(|| serde_json::json!({}));
    ns.as_object_mut().unwrap().insert(
        "kubernetes.io/os".to_string(),
        serde_json::Value::String("linux".to_string()),
    );
    let tols = kubernetes
        .entry("tolerations".to_string())
        .or_insert_with(|| serde_json::json!([]));
    tols.as_array_mut().unwrap().push(serde_json::json!({
        "key": "example.com/extra",
        "value": "yes",
        "effect": "NoSchedule",
    }));
    let config_str = serde_json::to_string(&config).expect("failed to serialize config");
    fs::write(repo.path().join(".rumpelpod.json"), config_str).unwrap();

    pod_command(&repo, &daemon)
        .args(["enter", "--create", "k8s-ns-test", "--", "true"])
        .success()
        .expect("rumpel enter with node-selector failed");

    // Verify both nodeSelector labels appear
    let kubectl_output = Command::new("kubectl")
        .args(["--context", context])
        .args(["--namespace", namespace])
        .args([
            "get",
            "pod",
            "-l",
            "rumpelpod/pod-name=k8s-ns-test",
            "-o",
            "jsonpath={.items[0].spec.nodeSelector}",
        ])
        .output()
        .expect("kubectl get failed");

    let node_selector = String::from_utf8_lossy(&kubectl_output.stdout);
    assert!(
        node_selector.contains("kubernetes.io/os"),
        "nodeSelector should include kubernetes.io/os: {}",
        node_selector,
    );

    // Verify our custom toleration is present
    let kubectl_output = Command::new("kubectl")
        .args(["--context", context])
        .args(["--namespace", namespace])
        .args([
            "get",
            "pod",
            "-l",
            "rumpelpod/pod-name=k8s-ns-test",
            "-o",
            "jsonpath={.items[0].spec.tolerations}",
        ])
        .output()
        .expect("kubectl get tolerations failed");

    let tolerations = String::from_utf8_lossy(&kubectl_output.stdout);
    assert!(
        tolerations.contains("example.com/extra"),
        "tolerations should include example.com/extra: {}",
        tolerations,
    );
}

// Historically disabled: after killing the tunnel-server, the daemon
// reconnects the tunnel but the git gateway port cached by the pod was
// stale, so git fetch failed with "Could not connect to server".  We
// restore this to verify current behavior.
#[test]
fn k8s_tunnel_reconnect() {
    println!("xtest:timeout=300");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let daemon = TestDaemon::start(&home);
    // procps provides pkill so we can kill the tunnel-server inside the pod.
    write_test_devcontainer(&repo, "RUN apk add --no-cache procps", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let pod_name = "reconnect-test";

    // First enter: creates pod and tunnel.
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "first"])
        .output()
        .expect("first enter failed to execute");
    assert!(
        output.status.success(),
        "first enter failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "first");

    // Find the actual K8s pod name.
    let kubectl_output = Command::new("kubectl")
        .args(["--context", &executor.context])
        .args(["--namespace", &executor.namespace])
        .args([
            "get",
            "pod",
            "-l",
            &format!("rumpelpod/pod-name={pod_name}"),
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ])
        .output()
        .expect("kubectl get pod failed");
    let k8s_pod_name = String::from_utf8_lossy(&kubectl_output.stdout)
        .trim()
        .to_string();
    assert!(
        !k8s_pod_name.is_empty(),
        "could not find k8s pod for {pod_name}",
    );

    // Kill the tunnel-server inside the pod so the mux task detects a broken pipe.
    let pkill_status = Command::new("kubectl")
        .args(["--context", &executor.context])
        .args(["--namespace", &executor.namespace])
        .args([
            "exec",
            &k8s_pod_name,
            "--",
            "pkill",
            "-f",
            "rumpel tunnel-server",
        ])
        .status()
        .expect("pkill exec failed");
    assert!(
        pkill_status.success(),
        "pkill should find and kill tunnel-server",
    );

    // Retry until the daemon notices the dead tunnel and reconnects.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let output = pod_command(&repo, &daemon)
            .args(["enter", pod_name, "--", "echo", "reconnected"])
            .output()
            .expect("retry enter failed to execute");

        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() == "reconnected"
        {
            break;
        }

        assert!(
            std::time::Instant::now() < deadline,
            "tunnel reconnection did not succeed within 60s, last stderr: {}",
            String::from_utf8_lossy(&output.stderr),
        );

        std::thread::sleep(Duration::from_millis(500));
    }
}

// K8s mirror of gateway_reconnect_push: a commit made while the daemon
// is down should be pushed to the host on the next reconnect,
// regardless of which pod is re-entered.  Kills the daemon, makes an
// offline commit inside pod A via kubectl exec, then creates pod B in
// the same cluster+namespace and asserts pod A's commit still lands
// on the host.  The daemon reconnects sibling k8s pods on any enter
// into the same cluster+namespace, so touching pod B heals pod A.
#[test]
fn k8s_gateway_reconnect_push() {
    println!("xtest:timeout=300");
    if !has_k8s_executor() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = k8s_executor(&home);
    let mut daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let pod_a = "reconnect-push-a";
    let pod_b = "reconnect-push-b";

    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_a, "--", "echo", "setup"])
        .success()
        .expect("initial enter of pod A failed");

    let host_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .success()
        .expect("rev-parse HEAD on host failed");
    let host_head = String::from_utf8_lossy(&host_head).trim().to_string();
    let expected_ref = format!("refs/rumpelpod/{pod_a}@{pod_a}");
    loop {
        let out = Command::new("git")
            .args(["rev-parse", &expected_ref])
            .current_dir(repo.path())
            .output()
            .expect("git rev-parse failed");
        if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == host_head {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // Find the actual K8s pod name for pod A.
    let kubectl_output = Command::new("kubectl")
        .args(["--context", &executor.context])
        .args(["--namespace", &executor.namespace])
        .args([
            "get",
            "pod",
            "-l",
            &format!("rumpelpod/pod-name={pod_a}"),
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ])
        .output()
        .expect("kubectl get pod failed");
    let k8s_pod_a = String::from_utf8_lossy(&kubectl_output.stdout)
        .trim()
        .to_string();
    assert!(!k8s_pod_a.is_empty(), "could not find k8s pod for {pod_a}");

    // Kill the daemon so the tunnel to pod A goes down.
    daemon.kill();

    // Create a commit inside pod A while the daemon is down.
    Command::new("kubectl")
        .args(["--context", &executor.context])
        .args(["--namespace", &executor.namespace])
        .args([
            "exec",
            &k8s_pod_a,
            "--",
            "env",
            "GIT_HTTP_LOW_SPEED_LIMIT=1",
            "GIT_HTTP_LOW_SPEED_TIME=10",
            "git",
            "-C",
            TEST_REPO_PATH,
            "commit",
            "--no-verify",
            "--allow-empty",
            "-m",
            "offline commit",
        ])
        .success()
        .expect("kubectl exec git commit failed");

    let rev_output = Command::new("kubectl")
        .args(["--context", &executor.context])
        .args(["--namespace", &executor.namespace])
        .args([
            "exec",
            &k8s_pod_a,
            "--",
            "git",
            "-C",
            TEST_REPO_PATH,
            "rev-parse",
            "HEAD",
        ])
        .success()
        .expect("kubectl exec git rev-parse failed");
    let offline_commit = String::from_utf8_lossy(&rev_output).trim().to_string();

    // The host should NOT have pod A's offline commit yet (daemon was down).
    let host_commit_before = Command::new("git")
        .args(["rev-parse", &expected_ref])
        .current_dir(repo.path())
        .output()
        .expect("git rev-parse failed");
    let host_commit_before = String::from_utf8_lossy(&host_commit_before.stdout)
        .trim()
        .to_string();
    assert_ne!(
        host_commit_before, offline_commit,
        "host should not have the offline commit yet",
    );

    // Restart the daemon and enter a *different* pod in the same
    // cluster+namespace.  Entering pod B should also reconnect pod A,
    // whose /events handler then pushes the offline commit.
    let daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_b, "--", "true"])
        .success()
        .expect("enter of pod B after restart failed");

    loop {
        let out = Command::new("git")
            .args(["rev-parse", &expected_ref])
            .current_dir(repo.path())
            .output()
            .expect("git rev-parse failed");
        if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == offline_commit {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
