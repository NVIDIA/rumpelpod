// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared test utilities and fixtures.

// Not all test files use all helpers, but we want them available.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use indoc::formatdoc;
use indoc::indoc;
use rumpelpod::config::{load_json_config, Host};
use rumpelpod::daemon::protocol::{
    ContainerId, Daemon, DaemonClient, EnsurePiConfigRequest, LaunchProgress, LaunchResult,
    PodLaunchParams, PodName,
};
use rumpelpod::CommandExt;
use tempfile::TempDir;

/// Standard test user name used in test images.
pub const TEST_USER: &str = "testuser";

/// Standard test user UID used in test images.
pub const TEST_USER_UID: u32 = 1007;

/// Standard repository path inside test containers.
pub const TEST_REPO_PATH: &str = "/home/testuser/workspace";

/// Environment variable used to configure the daemon socket path.
pub const SOCKET_PATH_ENV: &str = "RUMPELPOD_DAEMON_SOCKET";

/// Environment variable for XDG state directory (where rumpelpod data is stored).
const XDG_STATE_HOME_ENV: &str = "XDG_STATE_HOME";

/// Binaries every `TestHome` gets pre-linked into its `.local/bin`.
///
/// Split into two groups so the intent is legible:
/// - **rumpelpod core**: what rumpel itself and its tests drive
///   directly (`rumpel`, `git`, the shells, review's mock difftool).
/// - **shell plumbing**: utilities that POSIX shell scripts (notably
///   `git submodule` and friends) invoke internally.  They are not
///   the subject of any test, but the test daemon and `pod_command`
///   run with `PATH=bin_dir`, and missing them surfaces as cryptic
///   "command not found" failures deep inside git's own scripts.
const BASELINE_TOOLS: &[&str] = &[
    // rumpelpod core
    "rumpel", "git", "sh", "bash", "cat", "env",
    // shell plumbing for git/busybox-style scripts
    "awk", "basename", "chmod", "cp", "cut", "date", "dirname", "expr", "find", "grep", "head",
    "id", "ln", "ls", "mkdir", "mktemp", "mv", "printf", "readlink", "rm", "sed", "sleep", "sort",
    "tail", "tee", "test", "touch", "tr", "true", "false", "uname", "uniq", "wc", "which", "xargs",
];

/// Isolated HOME directory for a test.
///
/// Created before the executor setup and daemon so that both can write
/// into it (e.g. SSH config, Claude config) and the daemon inherits it
/// as `$HOME`.
///
/// The home ships with a `.local/bin` directory that acts as the
/// *entire* `$PATH` used by the daemon and any `rumpel` subprocess the
/// test spawns.  `TestHome::new` seeds it with [`BASELINE_TOOLS`];
/// executor setup and individual tests opt in to extras via
/// [`TestHome::link_local_bin`] so rumpelpod-specific tools like
/// `claude` or `kubectl` stay hidden unless explicitly requested.
pub struct TestHome {
    dir: TempDir,
}

impl TestHome {
    pub fn new() -> Self {
        let dir =
            TempDir::with_prefix("rumpelpod-test-home-").expect("Failed to create test home dir");
        let home = TestHome { dir };
        std::fs::create_dir_all(home.bin_dir()).expect("create .local/bin");
        home.link_local_bins(BASELINE_TOOLS);
        home
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Path to the per-test `.local/bin` directory that is used as
    /// the entire `$PATH` for the daemon and any rumpel subprocess.
    pub fn bin_dir(&self) -> PathBuf {
        self.dir.path().join(".local/bin")
    }

    /// Resolve `name` on the ambient test-process `PATH` and symlink it
    /// into [`TestHome::bin_dir`].  Panics if the binary cannot be
    /// found so tests fail loudly instead of silently hiding a tool.
    ///
    /// If the resolved file is a `#!/usr/bin/env <interpreter>` script
    /// (as Homebrew's `claude` and `codex` wrappers are on macOS), the
    /// interpreter is linked too.  Without it, exec of the wrapper
    /// would fail with `env: <interpreter>: No such file or directory`
    /// once the subprocess inherits `PATH=bin_dir`.
    pub fn link_local_bin(&self, name: &str) {
        let dst = self.bin_dir().join(name);
        // Idempotent: a tool may be in both the baseline and an
        // executor-specific extension, or requested again by a test.
        if dst.exists() || dst.symlink_metadata().is_ok() {
            return;
        }
        let src = find_on_local_path(name).unwrap_or_else(|| {
            panic!("binary {name:?} not found on PATH; cannot link into test home bin dir")
        });
        std::os::unix::fs::symlink(&src, &dst).unwrap_or_else(|e| {
            let src = src.display();
            let dst = dst.display();
            panic!("symlink {src} -> {dst}: {e}")
        });
        if let Some(interp) = env_shebang_interpreter(&src) {
            self.link_local_bin(&interp);
        }
    }

    pub fn link_local_bins(&self, names: &[&str]) {
        for name in names {
            self.link_local_bin(name);
        }
    }

    /// Build a supplementary bin directory that callers can prepend
    /// to the client half's `$PATH` without the daemon ever seeing
    /// the named binaries.  Used by tests that exercise the asymmetric
    /// "client resolves a host binary and hands its path to the
    /// daemon over IPC" path (e.g. `find_local_claude_cli`).
    ///
    /// `#!/usr/bin/env <interp>` shebangs are followed, but the
    /// interpreter (e.g. `node` for Homebrew's `claude` wrapper) is
    /// linked into the daemon-visible [`TestHome::bin_dir`] instead
    /// of the client-only dir: the daemon has to exec the absolute
    /// path returned by `find_local_claude_cli`, and the kernel looks
    /// up the shebang's `env` interpreter against the daemon's own
    /// `PATH`, not whatever the client passed.
    pub fn client_only_bin_dir(&self, tools: &[&str]) -> PathBuf {
        let dir = self.dir.path().join("client-only-bin");
        std::fs::create_dir_all(&dir).expect("create client-only-bin");
        for name in tools {
            let dst = dir.join(name);
            if dst.exists() || dst.symlink_metadata().is_ok() {
                continue;
            }
            let src = find_on_local_path(name).unwrap_or_else(|| {
                panic!("binary {name:?} not found on PATH; cannot seed client-only bin dir")
            });
            std::os::unix::fs::symlink(&src, &dst).unwrap_or_else(|e| {
                let src = src.display();
                let dst = dst.display();
                panic!("symlink {src} -> {dst}: {e}")
            });
            if let Some(interp) = env_shebang_interpreter(&src) {
                self.link_local_bin(&interp);
            }
        }
        dir
    }

    /// Symlink every regular file from `dir` into the bin dir.
    ///
    /// Used to pull docker credential helpers (e.g.
    /// `docker-credential-ecr-login`) from the directory that
    /// `xtest.rs` resolved for the EKS executor.
    pub fn link_local_bins_from_dir(&self, dir: &Path) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let dst = self.bin_dir().join(&name);
            if dst.exists() || dst.symlink_metadata().is_ok() {
                continue;
            }
            std::os::unix::fs::symlink(entry.path(), &dst).unwrap_or_else(|e| {
                let src = entry.path().display().to_string();
                let dst = dst.display();
                panic!("symlink {src} -> {dst}: {e}")
            });
        }
    }
}

/// Locate `name` on the ambient `$PATH` of the test process.  This is
/// deliberately separate from the rumpelpod daemon's later `$PATH`:
/// we want to resolve host binaries *before* narrowing the PATH for
/// the subprocess.
fn find_on_local_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// If `path` starts with `#!/usr/bin/env <interpreter>`, return the
/// interpreter name.  Used so that linking a Homebrew `claude`
/// wrapper also pulls `node` into the test bin dir.  Only `env`-style
/// shebangs are followed: absolute-path shebangs (e.g. `#!/bin/sh`)
/// already resolve without PATH so they need nothing extra.
fn env_shebang_interpreter(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 256];
    let n = file.read(&mut buf).ok()?;
    let head = std::str::from_utf8(&buf[..n]).ok()?;
    let first_line = head.split('\n').next()?;
    let rest = first_line.strip_prefix("#!")?.trim_start();
    let rest = rest.strip_prefix("/usr/bin/env")?.trim_start();
    // env can take flags like `-S`; we only care about the plain
    // `env <interp> ...` form, which is what Homebrew wrappers use.
    let first_word = rest.split_whitespace().next()?;
    if first_word.starts_with('-') {
        return None;
    }
    Some(first_word.to_string())
}

/// A test daemon that manages pods for integration tests.
///
/// Uses the [`TestHome`] as `$HOME` and stores its socket, state, and
/// runtime directories under it.  On drop the daemon process is killed.
pub struct TestDaemon {
    pub socket_path: PathBuf,
    /// Path to the bin dir used as the daemon's `$PATH`.  Kept so
    /// `pod_command` can point rumpel subprocesses at the same narrow
    /// PATH the daemon runs with.
    pub bin_dir: PathBuf,
    /// Path to the isolated test home the daemon runs under.  Kept
    /// so `pod_command` can set `HOME` on client-side rumpel
    /// subprocesses -- otherwise they inherit the ambient developer
    /// home and lookups in `~/.docker/` (buildx instances, docker
    /// config.json) miss the executor-specific files that
    /// `ExecutorResources`/`k8s_executor` copy into the test home.
    pub home_path: PathBuf,
    process: Child,
    /// Separate short-path temp dir for the runtime directory on macOS,
    /// where Unix socket paths must be under 104 bytes.
    #[allow(dead_code)]
    runtime_temp_dir: Option<TempDir>,
}

impl TestDaemon {
    pub fn start(home: &TestHome) -> Self {
        Self::start_inner(home)
    }

    /// Start a daemon whose `$PATH` additionally includes the host
    /// `claude` and `codex` binaries.
    ///
    /// Most tests should use [`Self::start`] so the daemon does not
    /// pre-install these large CLIs into every prepared image.  Tests
    /// that specifically exercise the host-CLI detection path call
    /// this helper instead.
    pub fn start_with_local_llm_clis(home: &TestHome) -> Self {
        home.link_local_bins(&["claude", "codex"]);
        Self::start_inner(home)
    }

    /// Start a daemon whose `$PATH` additionally includes the host
    /// `grok` binary, so the prepared-image build detects it and bakes
    /// the Grok CLI into the pod image.  See `start_with_local_llm_clis`.
    pub fn start_with_local_grok(home: &TestHome) -> Self {
        home.link_local_bins(&["grok"]);
        Self::start_inner(home)
    }

    fn start_inner(home: &TestHome) -> Self {
        let home_path = home.path();
        let socket_path = home_path.join("rumpelpod.sock");
        let state_dir = home_path.join("state");

        // macOS limits Unix socket paths to 104 bytes. The default TMPDIR
        // on macOS is ~51 chars, making our socket paths too long. Use a
        // short-prefix temp dir under /tmp for the runtime directory.
        let (runtime_dir, runtime_temp_dir) = if cfg!(target_os = "macos") {
            let rt =
                TempDir::with_prefix_in("rp-", "/tmp").expect("Failed to create runtime temp dir");
            let path = rt.path().to_path_buf();
            (path, Some(rt))
        } else {
            (home_path.join("runtime"), None)
        };

        // Ensure runtime directory exists, including the 'rumpelpod' subdirectory that
        // the daemon expects for the git socket.
        std::fs::create_dir_all(runtime_dir.join("rumpelpod"))
            .expect("Failed to create runtime dir");

        // Resolve the Docker socket while HOME still points at the real
        // home directory. The daemon subprocess gets a fake HOME, so
        // default_docker_socket() inside it would miss sockets under the
        // real home (e.g. Colima on macOS). Passing DOCKER_HOST lets the
        // daemon find it regardless.
        let docker_socket = rumpelpod::daemon::default_docker_socket();
        let docker_socket_display = docker_socket.display();
        let docker_host = format!("unix://{docker_socket_display}");

        // Copy the real user's Docker config so the daemon process can
        // find CLI plugins (e.g. buildx installed via homebrew on macOS).
        // When the executor already placed a docker config (e.g. with
        // ECR credential helper for k8s tests), merge cliPluginsExtraDirs
        // from the real config so the buildx plugin is still found.
        let dst_docker_config = home_path.join(".docker/config.json");
        if !dst_docker_config.exists() {
            if let Some(real_home) = dirs::home_dir() {
                let src = real_home.join(".docker/config.json");
                if src.exists() {
                    let dst_dir = home_path.join(".docker");
                    std::fs::create_dir_all(&dst_dir).expect("creating test .docker dir");
                    std::fs::copy(&src, dst_dir.join("config.json"))
                        .expect("copying docker config.json");
                }
            }
        } else if let Some(real_home) = dirs::home_dir() {
            // Executor placed its own config; merge cliPluginsExtraDirs
            // from the user's real config so Docker finds homebrew plugins.
            let src = real_home.join(".docker/config.json");
            if src.exists() {
                if let (Ok(real_json), Ok(dst_json)) = (
                    std::fs::read_to_string(&src),
                    std::fs::read_to_string(&dst_docker_config),
                ) {
                    if let (Ok(real_val), Ok(mut dst_val)) = (
                        serde_json::from_str::<serde_json::Value>(&real_json),
                        serde_json::from_str::<serde_json::Value>(&dst_json),
                    ) {
                        if let Some(dirs) = real_val.get("cliPluginsExtraDirs") {
                            dst_val
                                .as_object_mut()
                                .unwrap()
                                .insert("cliPluginsExtraDirs".to_string(), dirs.clone());
                            std::fs::write(
                                &dst_docker_config,
                                serde_json::to_string_pretty(&dst_val).unwrap(),
                            )
                            .expect("merging cliPluginsExtraDirs into docker config");
                        }
                    }
                }
            }
        }

        let bin_dir = home.bin_dir();

        let mut cmd = Command::new("rumpel");
        cmd.env("HOME", home_path)
            .env("PATH", &bin_dir)
            .env("DOCKER_HOST", &docker_host)
            .env(SOCKET_PATH_ENV, &socket_path)
            .env(XDG_STATE_HOME_ENV, &state_dir)
            .env("XDG_RUNTIME_DIR", &runtime_dir);

        // Enable the LLM cache proxy on the git HTTP server so
        // containers can route API requests through the tunnel.
        if std::env::var("RUMPELPOD_TEST_LLM_OFFLINE").is_err() {
            cmd.env("RUMPELPOD_TEST_LLM_OFFLINE", "1");
        }

        let process = cmd
            .arg("daemon")
            .stdin(Stdio::null())
            .spawn_with_logging("DAEMON")
            .expect("Failed to spawn daemon");

        // Wait for socket to exist
        let timeout = std::time::Duration::from_secs(10);
        let start = std::time::Instant::now();
        while !socket_path.exists() {
            if start.elapsed() > timeout {
                panic!(
                    "Daemon socket did not appear within {:?}: {}",
                    timeout,
                    socket_path.display()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        TestDaemon {
            socket_path,
            bin_dir,
            home_path: home_path.to_path_buf(),
            process,
            runtime_temp_dir,
        }
    }

    /// URL of the daemon-side LLM cache proxy for `provider` (e.g.
    /// "anthropic").  The daemon writes its localhost git HTTP server
    /// port to a sibling of the socket at startup when the cache
    /// proxy is mounted; this reads it and returns the full base URL
    /// a host-side CLI should use as `ANTHROPIC_BASE_URL` / etc.  The
    /// proxy handler skips auth on this route because it is only
    /// mounted in test mode and the server binds to loopback.
    pub fn llm_cache_proxy_url(&self, provider: &str) -> String {
        let port_path = self
            .socket_path
            .parent()
            .expect("socket path has a parent")
            .join("llm-cache-proxy-port");
        let port = std::fs::read_to_string(&port_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", port_path.display()));
        format!(
            "http://127.0.0.1:{}/llm-cache-proxy/{provider}",
            port.trim()
        )
    }

    /// Kill the daemon process and wait for it to exit.
    pub fn kill(&mut self) {
        self.process.kill().expect("failed to kill daemon");
        self.process.wait().expect("failed to wait for daemon");
        // Remove the socket so that a subsequent start() on the same home
        // directory does not see a stale file and return before the new
        // daemon is ready.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        // Delete all pods through the daemon so cleanup works for all
        // executor types (Docker, SSH, K8s).
        let client = DaemonClient::new_unix(&self.socket_path);
        let _ = client.delete_all_pods();

        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// A temporary directory initialized as a git repository for tests, cleaned up on drop.
pub struct TestRepo {
    dir: TempDir,
}

impl TestRepo {
    pub fn new() -> Self {
        Self::new_with_prefix("rumpelpod-test-repo-")
    }

    /// Create a test repo whose temp directory name starts with `prefix`.
    /// Useful for testing behavior when the repo path contains special characters.
    pub fn new_with_prefix(prefix: &str) -> Self {
        let dir = TempDir::with_prefix(prefix).expect("Failed to create temp directory");

        // Initialize as a git repository with an initial commit
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .success()
            .expect("git init failed");

        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .success()
            .expect("git config user.email failed");

        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir.path())
            .success()
            .expect("git config user.name failed");

        create_commit(dir.path(), "Initial commit");

        TestRepo { dir }
    }

    /// Create a temporary directory without git initialization.
    /// Useful for testing behavior outside of a git repository.
    pub fn new_without_git() -> Self {
        let dir =
            TempDir::with_prefix("rumpelpod-test-repo-").expect("Failed to create temp directory");
        TestRepo { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        // Pod cleanup is handled by TestDaemon::drop, which calls
        // delete_all_pods through the daemon.  This works for all
        // executor types (Docker, SSH, K8s).
    }
}

/// Create a commit with a fixed timestamp to ensure deterministic directory hashes.
pub fn create_commit(repo_path: &Path, message: &str) {
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", message])
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00")
        .current_dir(repo_path)
        .success()
        .expect("git commit failed");
}

/// Create a Command for the rumpel binary, pre-configured for testing.
///
/// Matches the daemon's own narrowed `$PATH` and uses the same
/// isolated `$HOME`, so client-side lookups (e.g.
/// `find_local_claude_cli`) see the same tools the daemon does and
/// client-side subprocesses (e.g. `docker buildx --builder` for
/// `rumpel hub install`) pick up the executor-specific docker /
/// buildx config that `ExecutorResources` / `k8s_executor` copy into
/// the test home rather than the ambient developer home.
pub fn pod_command(repo: &TestRepo, daemon: &TestDaemon) -> Command {
    let mut cmd = Command::new("rumpel");
    cmd.current_dir(repo.path())
        .env("PATH", &daemon.bin_dir)
        .env("HOME", &daemon.home_path)
        .env(SOCKET_PATH_ENV, &daemon.socket_path);

    // Default to offline mode for tests unless explicitly configured.
    // This ensures tests don't accidentally depend on ambient API keys.
    if std::env::var("RUMPELPOD_TEST_LLM_OFFLINE").is_err() {
        cmd.env("RUMPELPOD_TEST_LLM_OFFLINE", "1");
    }

    cmd
}

pub fn launch_pod_via_daemon(repo: &TestRepo, daemon: &TestDaemon, pod_name: &str) -> LaunchResult {
    let client = DaemonClient::new_unix(&daemon.socket_path);
    let pod_name = PodName::new(pod_name.to_string()).expect("valid pod name");
    let mut progress = client
        .launch_pod(PodLaunchParams {
            pod_name,
            repo_path: repo.path().to_path_buf(),
            host_branch: current_branch(repo.path()),
            host: resolve_test_host(repo.path()),
            git_identity: None,
            claude_cli_path: None,
            codex_cli_path: None,
            pi_cli_path: None,
            grok_cli_path: None,
            inject_system_prompt: load_json_config(repo.path())
                .expect("load .rumpelpod.json")
                .inject_system_prompt,
            description_file: None,
            local_env_vars: HashMap::new(),
            ssh_auth_sock: None,
        })
        .expect("launch pod request failed");
    for _line in &mut progress {}
    progress.finish().expect("pod launch failed")
}

pub fn ensure_pi_config_via_daemon(
    repo: &TestRepo,
    daemon: &TestDaemon,
    pod_name: &str,
    launch: &LaunchResult,
) {
    let client = DaemonClient::new_unix(&daemon.socket_path);
    let pod_name = PodName::new(pod_name.to_string()).expect("valid pod name");
    client
        .ensure_pi_config(EnsurePiConfigRequest {
            pod_name,
            repo_path: repo.path().to_path_buf(),
            container_id: ContainerId(launch.container_id.0.clone()),
            container_url: launch.container_url.clone(),
            container_token: launch.container_token.clone(),
            trust_workspace: true,
        })
        .expect("ensure pi config failed");
}

pub fn write_test_devcontainer_with_fake_pi(repo: &TestRepo) {
    let extra_dockerfile = indoc! {r#"
        RUN mkdir -p /opt/rumpelpod/bin
        RUN printf '%s\n' '#!/bin/sh' 'printf "fake pi %s\n" "$*"' > /opt/rumpelpod/bin/pi
        RUN chmod 755 /opt/rumpelpod/bin/pi
    "#};
    write_test_devcontainer(repo, extra_dockerfile, "");
}

pub fn write_host_pi_settings(home: &TestHome, source: &str) {
    let agent_dir = home.path().join(".pi/agent");
    std::fs::create_dir_all(&agent_dir).expect("create host .pi/agent");
    std::fs::write(
        agent_dir.join("settings.json"),
        format!(r#"{{"defaultProjectTrust":"ask","source":"{source}"}}"#),
    )
    .expect("write host pi settings");
}

fn current_branch(repo_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(repo_path)
        .output()
        .expect("git branch --show-current failed");
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

fn resolve_test_host(repo_path: &Path) -> Host {
    let config = load_json_config(repo_path).expect("load .rumpelpod.json");
    if let Some(host) = config.host {
        return Host::parse(&host).expect("parse test host");
    }
    if let Some(kubernetes) = config.kubernetes {
        return Host::Kubernetes {
            context: kubernetes.context,
            namespace: kubernetes
                .namespace
                .unwrap_or_else(|| "default".to_string()),
            registry: kubernetes.registry,
            node_selector: kubernetes.node_selector,
            tolerations: kubernetes.tolerations,
            builder: kubernetes.builder,
        };
    }
    Host::Localhost
}

/// Write a standard test devcontainer.json with a Dockerfile build section.
///
/// Creates a Dockerfile that installs git, creates the test user, and
/// COPYs the repo.  The devcontainer.json uses a build section so the
/// image is built on first `rumpel enter` (with buildkit layer caching).
///
/// `extra_dockerfile` is inserted after COPY (user and workspace exist)
/// but before USER, so commands still run as root.
/// `extra_json` is spliced into the devcontainer.json object (include
/// a leading comma if non-empty).
pub fn write_test_devcontainer(repo: &TestRepo, extra_dockerfile: &str, extra_json: &str) {
    let devcontainer_dir = repo.path().join(".devcontainer");
    std::fs::create_dir_all(&devcontainer_dir).expect("Failed to create .devcontainer dir");

    // Mark safe.directory so extra_dockerfile can run git as root on
    // the testuser-owned workspace without "dubious ownership" errors.
    let dockerfile = formatdoc! {r#"
        FROM cgr.dev/chainguard/wolfi-base
        RUN apk add --no-cache git bash shadow coreutils openssh-client
        RUN useradd -m -u {TEST_USER_UID} -s /bin/bash {TEST_USER}
        COPY --chown={TEST_USER}:{TEST_USER} . {TEST_REPO_PATH}
        RUN git config --global --add safe.directory {TEST_REPO_PATH}
        {extra_dockerfile}
        USER {TEST_USER}
    "#};
    std::fs::write(devcontainer_dir.join("Dockerfile"), dockerfile)
        .expect("Failed to write Dockerfile");

    let devcontainer_json = formatdoc! {r#"
        {{
            "build": {{
                "dockerfile": "Dockerfile",
                "context": ".."
            }},
            "workspaceFolder": "{TEST_REPO_PATH}"{extra_json}
        }}
    "#};
    std::fs::write(
        devcontainer_dir.join("devcontainer.json"),
        devcontainer_json,
    )
    .expect("Failed to write devcontainer.json");
}
