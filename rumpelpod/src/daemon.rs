// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod db;
pub mod host_connection;
pub mod protocol;
pub mod reconnect;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::image::OutputLine;
use anyhow::{Context, Result};
use listenfd::ListenFd;
use log::error;
use rusqlite::Connection;
use tokio::net::UnixListener;

use sha2::{Digest, Sha256};

use crate::async_runtime::block_on;
use crate::config::Host;
use crate::devcontainer::{
    self, check_no_unresolved_mount_vars, compute_devcontainer_id, substitute_vars, BuildOptions,
    DevContainer, GpuRequirement, HostRequirements, MountType, Port, PortAttributes,
    SubstitutionContext,
};
use crate::gateway;
use crate::git_http_server::{GitHttpServer, SharedGitServerState};
use host_connection::{HostConnectionEvent, HostConnectionEventRx, HostConnectionRegistry};
use protocol::{
    AddForwardedPortRequest, ContainerId, Daemon, EnsureClaudeConfigRequest, EnsurePiConfigRequest,
    ForkPodRequest, Image, LaunchResult, PodInfo, PodLaunchParams, PodName, PodStatus, PortInfo,
};
use reconnect::PodEventManager;

use crate::pod::types::{ClaudeState, CodexState, GitSetupParams};
use crate::pod::PodClient;
use crate::RetryPolicy;

/// Environment variable to override the daemon socket path for testing.
pub const SOCKET_PATH_ENV: &str = "RUMPELPOD_DAEMON_SOCKET";

/// Returns the local Docker socket path.
/// Checks DOCKER_HOST first, then probes standard socket locations.
pub fn default_docker_socket() -> PathBuf {
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if let Some(path) = host.strip_prefix("unix://") {
            return PathBuf::from(path);
        }
    }
    let mut candidates = vec![PathBuf::from("/var/run/docker.sock")];
    if let Some(home) = dirs::home_dir() {
        // Docker Desktop (macOS 4.13+)
        candidates.push(home.join(".docker/run/docker.sock"));
        // Colima
        candidates.push(home.join(".colima/default/docker.sock"));
        // Rancher Desktop
        candidates.push(home.join(".rd/docker.sock"));
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("/var/run/docker.sock"))
}

/// Build a `BuildOutputFn` that forwards every line to `tx`.
///
/// `BuildOutputFn` is a one-shot boxed closure (consumed by the
/// single build it drives), so callers that run multiple builds
/// through the same progress channel need a fresh callback per step.
fn make_build_output(
    tx: &std::sync::mpsc::Sender<crate::image::OutputLine>,
) -> Option<crate::image::BuildOutputFn> {
    let tx = tx.clone();
    Some(Box::new(move |line: crate::image::OutputLine| {
        // Send failure means the client disconnected mid-build.
        // The caller notices when the progress iterator drains.
        if tx.send(line).is_err() {
            log::debug!("build output channel closed");
        }
    }) as crate::image::BuildOutputFn)
}

/// Parse the source pod's stored devcontainer.json into a fully-resolved
/// `DevContainer`, applying the same `${localEnv:...}` resolution as
/// `load_and_resolve_devcontainer` but skipping the host-disk read.
///
/// `--env-file` resolution is intentionally NOT re-run here.  Forks
/// inherit container env vars by snapshotting the source pod's running
/// process environment via its `/container-env` endpoint; that gives
/// the value the source actually saw, independent of any later edits
/// to `.env` on disk.  The caller layers that snapshot onto
/// `dc.container_env` after this returns.
fn parse_prebuilt_devcontainer(
    json: &str,
    repo_path: &Path,
    pod_name: &str,
    local_env: &HashMap<String, String>,
) -> Result<DevContainer> {
    let devcontainer: DevContainer =
        json5::from_str(json).context("parsing source pod's stored devcontainer.json")?;
    Ok(resolve_devcontainer_vars(
        devcontainer,
        repo_path,
        pod_name,
        local_env,
    ))
}

/// Sorted list of the resolved `containerEnv` key names (after
/// `--env-file` resolution).  Baked into the prepared image as
/// `/opt/rumpelpod/container-env-keys` so the pod server can scope
/// `/container-env` to keys the daemon actually set.
fn container_env_keys_sorted(dc: &DevContainer) -> Vec<String> {
    let mut keys: Vec<String> = dc
        .container_env
        .as_ref()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

/// Preflight check that `image` is present on the docker host.
/// Forks fail fast here rather than fall back to a rebuild: the user
/// explicitly opted into "reuse the source's exact image".  The
/// kubernetes path of `image_present` trivially returns true (the
/// cluster's image inventory is not globally visible to the client),
/// so this check is effectively docker-only.
fn preflight_image_present(executor: &crate::executor::Executor, image: &str) -> Result<()> {
    if executor.image_present(image)? {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "image '{image}' is not present on the docker host.\n\
             Recreate the source pod first if you want to fork from it."
        ))
    }
}

/// Build a `GitSetupParams` for a fresh pod: one branch named after
/// the pod, based on `host/HEAD`, optionally tracking `host/<host_branch>`.
/// Forks build their own params with multiple branches and an
/// `extra_host_fetch` refspec.
fn fresh_pod_git_setup(
    pod_name: &str,
    host_branch: Option<&str>,
    git_identity: Option<crate::git::GitIdentity>,
) -> GitSetupParams {
    GitSetupParams {
        branches: vec![crate::pod::git_setup::GitSetupBranch {
            name: pod_name.to_string(),
            base: "host/HEAD".to_string(),
            upstream: host_branch.map(|b| format!("host/{b}")),
        }],
        primary: pod_name.to_string(),
        extra_host_fetch: Vec::new(),
        git_identity,
    }
}

/// Inverse of `serialize_local_env`: parses the JSON array of
/// "KEY=VALUE" strings stored at pod creation back into a HashMap.
fn deserialize_local_env(json: &str) -> Result<HashMap<String, String>> {
    let entries: Vec<String> =
        serde_json::from_str(json).context("parsing local_env from source pod row")?;
    let mut map = HashMap::new();
    for entry in entries {
        if let Some((k, v)) = entry.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    Ok(map)
}

/// Translate a source-pod branch's upstream for use on the fork.
///
/// Rules:
/// - `host/<x>` is preserved.
/// - `rumpelpod/<S>` (the source's primary shortcut) becomes
///   `rumpelpod/<N>`; same with `@<branch>` suffix.
/// - `rumpelpod/<other>` (any other pod's branch) is preserved.
/// - A local upstream named `<S>` (the source's primary, by name)
///   becomes local `<N>`.
/// - A local upstream that names another branch in the same pod is
///   preserved iff that branch is also being copied.  Otherwise dropped.
/// - `None` stays `None`.
fn rewrite_upstream(
    upstream: Option<&str>,
    source: &str,
    new_name: &str,
    branch_set: &std::collections::HashSet<String>,
) -> Option<String> {
    let upstream = upstream?;
    if let Some(rest) = upstream.strip_prefix("host/") {
        let _ = rest;
        return Some(upstream.to_string());
    }
    if let Some(rest) = upstream.strip_prefix("rumpelpod/") {
        // "rumpelpod/<pod>" or "rumpelpod/<branch>@<pod>"
        let (pod, branch) = match rest.rsplit_once('@') {
            Some((br, pd)) => (pd, Some(br)),
            None => (rest, None),
        };
        if pod == source {
            return Some(match branch {
                Some(b) => format!("rumpelpod/{b}@{new_name}"),
                None => format!("rumpelpod/{new_name}"),
            });
        }
        return Some(upstream.to_string());
    }
    // Local upstream: a bare branch name with no remote prefix.
    if upstream == source {
        return Some(new_name.to_string());
    }
    if branch_set.contains(upstream) {
        return Some(upstream.to_string());
    }
    eprintln!(
        "fork: dropping upstream '{upstream}' (refers to a local branch \
         that is not being copied)"
    );
    None
}

/// Agents whose home-relative state is transferred between pods.
/// Mirrors the registry in `pod::server::agent_paths`; kept here as a
/// flat list because recreate / fork iterate over it.
pub(crate) const AGENT_NAMES: &[&str] = &["claude", "codex", "grok", "pi"];

/// Buffer the tar body from GET /agent-files/<agent> into memory.
/// Returns None if the agent has no state to transfer.  Used by
/// recreate (the old pod is deleted before the new one exists, so
/// streaming is impossible) and by `rumpel fork` (the new pod does
/// not yet exist when we read from the source).
///
/// The body is whatever the transport layer hands us -- typically
/// already-decompressed plain tar -- so we just read it through.
/// The subsequent put_agent_files call applies its own gzip on the
/// upload side.
fn snapshot_agent_files(pod: &PodClient, agent: &str) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut reader = match pod.get_agent_files(agent)? {
        Some(r) => r,
        None => return Ok(None),
    };
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .context("reading agent-files body")?;
    Ok(Some(buf))
}

/// Serialize the --local-env vars as a JSON array of "KEY=VALUE" strings.
///
/// HashMap iteration order is unstable, so sort to keep DB rows
/// reproducible (a fork that inherits this string compares cleanly).
fn serialize_local_env(local_env: &HashMap<String, String>) -> String {
    let mut entries: Vec<String> = local_env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    entries.sort();
    serde_json::to_string(&entries).expect("Vec<String> is serializable")
}

/// Get the daemon socket path.
/// Uses $RUMPELPOD_DAEMON_SOCKET if set, then $XDG_RUNTIME_DIR/rumpelpod.sock,
/// then /tmp/rumpelpod-<uid>/rumpelpod.sock as fallback.
pub fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(SOCKET_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/rumpelpod-{uid}"))
        });
    Ok(runtime_dir.join("rumpelpod.sock"))
}

/// Compute the local-machine directory for a pod's ssh-agent socket.
///
/// Uses the same runtime directory logic as `socket_path()`, placing agent
/// sockets under `<runtime_dir>/rumpelpod/agents/<hash>/`.  The directory
/// name is a short hash to stay within the Unix socket path length limit
/// (108 bytes on Linux).
pub fn ssh_agent_dir(repo_path: &Path, pod_name: &PodName) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/rumpelpod-{uid}"))
        });

    let mut hasher = Sha256::new();
    hasher.update(repo_path.as_os_str().as_encoded_bytes());
    hasher.update(pod_name.0.as_bytes());
    let hash = hex::encode(hasher.finalize());
    let hash_prefix = &hash[..12];

    runtime_dir.join("rumpelpod/agents").join(hash_prefix)
}

/// Handle to a running ssh-agent process managed by the daemon.
struct SshAgentHandle {
    child: std::process::Child,
}

impl Drop for SshAgentHandle {
    fn drop(&mut self) {
        if let Err(e) = self.child.kill() {
            eprintln!("warning: failed to kill ssh-agent: {e}");
        }
        if let Err(e) = self.child.wait() {
            eprintln!("warning: failed to wait on ssh-agent: {e}");
        }
    }
}

#[derive(Clone)]
pub struct DaemonServer {
    /// SQLite connection for pod metadata.
    db: Arc<Mutex<Connection>>,
    /// Port the localhost git HTTP server is listening on (tunnel target).
    localhost_server_port: u16,
    /// Per-host connection objects (one per localhost / ssh remote /
    /// k8s cluster).  Tracks SSH liveness and cached kube clients;
    /// emits Connected/Disconnected events on the registry's central
    /// channel.
    host_connections: Arc<HostConnectionRegistry>,
    /// Per-pod event listeners that maintain SSE connections to pod servers.
    pod_events: Arc<PodEventManager>,
    /// Active exec-proxy listeners implementing devcontainer
    /// `forwardPorts`.  One `ExecProxyHandle` per forwarded port;
    /// dropping the Vec tears them all down.  Executor-agnostic:
    /// used for both docker and k8s pods.
    #[allow(clippy::type_complexity)]
    port_forward_proxies:
        Arc<Mutex<HashMap<(PathBuf, String), Vec<crate::exec_proxy::ExecProxyHandle>>>>,
    /// Active tunnel handles for Kubernetes pods.
    /// Each tunnel multiplexes TCP over kubectl exec so the pod can reach
    /// the host's git HTTP server on a loopback port.
    #[allow(clippy::type_complexity)]
    k8s_tunnels: Arc<Mutex<HashMap<(PathBuf, String), crate::tunnel::TunnelHandle>>>,
    /// Active tunnel handles for Docker containers.
    /// Each tunnel multiplexes TCP over docker exec so the container can
    /// reach the host's git HTTP server on a loopback port.
    #[allow(clippy::type_complexity)]
    docker_tunnels: Arc<Mutex<HashMap<(PathBuf, String), crate::tunnel::TunnelHandle>>>,
    /// Active exec proxy handles for Docker containers.
    /// Each proxy routes HTTP to container-serve via docker exec instead
    /// of bridge IPs or SSH port forwards.
    #[allow(clippy::type_complexity)]
    exec_proxies: Arc<Mutex<HashMap<(PathBuf, String), crate::exec_proxy::ExecProxyHandle>>>,
    /// Per-pod ssh-agent processes running on the local machine.
    /// The agent socket is relayed into containers via WebSocket.
    #[allow(clippy::type_complexity)]
    ssh_agents: Arc<Mutex<HashMap<(PathBuf, String), SshAgentHandle>>>,
    /// Per repo/pod local TCP listeners that bridge the codex TUI's
    /// `--remote ws://...` connection through to the pod server's
    /// /codex endpoint (with the pod's bearer token).  The codex TUI
    /// runs as a child of the daemon-managed screen session below; it
    /// dials this loopback port instead of going through the daemon's
    /// Unix socket because we don't control the codex CLI.
    #[allow(clippy::type_complexity)]
    codex_proxies: Arc<Mutex<HashMap<(PathBuf, String), CodexProxyHandle>>>,
    /// Screen-style PTY sessions managed by the daemon (currently only
    /// codex).  Sessions are keyed by name (repo path plus pod name) and
    /// outlive any single client connection so the user can detach
    /// (Ctrl-a d) and reattach to the same TUI process.
    pty_sessions: crate::pty_session::PtySessions,
}

/// Sanitize a string for use as a container hostname (RFC 1123):
/// alphanumeric and hyphens only, max 63 chars, cannot start/end with hyphen.
fn sanitize_hostname(s: &str) -> String {
    let sanitized: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let trimmed = sanitized.trim_matches('-');

    if trimmed.is_empty() {
        "pod".to_string()
    } else if trimmed.len() > 63 {
        trimmed[..63].trim_end_matches('-').to_string()
    } else {
        trimmed.to_string()
    }
}

/// Dockerfile used when a project has no devcontainer.json.
const DEFAULT_DOCKERFILE: &str = indoc::indoc! {"
    FROM debian:testing

    RUN apt-get update \
     && apt-get install -y --no-install-recommends \
            ca-certificates \
            curl \
            git \
            jq \
            less \
            openssh-client \
            sudo \
            unzip \
            vim \
            wget

    RUN useradd -m -s /bin/bash user \
     && echo 'user ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/user
    USER user
"};

/// Write the embedded default Dockerfile to a fresh tempdir and
/// populate `devcontainer.build` to point at it.  The returned
/// `TempDir` must outlive the subsequent `resolve_image` call --
/// `compute_image_tag` walks the context and `docker build` reads
/// from it.  The context *path* no longer participates in the image
/// tag, so tempdir churn does not burn the image cache.
fn stage_default_image(devcontainer: &mut DevContainer) -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir().context("creating default image tempdir")?;
    let dockerfile = dir.path().join("Dockerfile");
    fs::write(&dockerfile, DEFAULT_DOCKERFILE)
        .with_context(|| format!("writing {}", dockerfile.display()))?;
    devcontainer.build = Some(BuildOptions {
        dockerfile: Some(dockerfile.to_string_lossy().to_string()),
        context: Some(dir.path().to_string_lossy().to_string()),
        ..Default::default()
    });
    Ok(dir)
}

/// Load devcontainer.json from `repo_path`, fully resolve all variables
/// using the client-provided `${localEnv:...}` map, normalize build
/// paths, inline `--env-file` runArgs, and fall back to a built-in
/// default image when the repo has no devcontainer.json.
///
/// The daemon does this itself so the client can send just the repo
/// path and a localEnv map over the wire.
///
/// Returns `used_default_image = true` when no image or build was
/// configured; the caller is responsible for emitting a user-visible
/// warning via the progress channel.
fn load_and_resolve_devcontainer(
    repo_path: &Path,
    pod_name: &str,
    local_env: &HashMap<String, String>,
) -> Result<(DevContainer, bool)> {
    let (mut devcontainer, devcontainer_dir) = DevContainer::find_and_load(repo_path)?
        .unwrap_or_else(|| (DevContainer::default(), repo_path.to_path_buf()));

    let used_default_image = devcontainer.image.is_none() && !devcontainer.has_build();
    // The default-image fallback deliberately happens later in
    // `launch_pod_impl`, past the reentry check: a reconnect does
    // not need the embedded Dockerfile staged at all.
    if !used_default_image {
        devcontainer.resolve_build_paths(&devcontainer_dir, repo_path);
    }

    devcontainer
        .resolve_env_files(repo_path)
        .context("resolving --env-file from runArgs")?;

    let devcontainer = resolve_devcontainer_vars(devcontainer, repo_path, pod_name, local_env);
    Ok((devcontainer, used_default_image))
}

/// Format a single-line `hostRequirements: ...` summary for the progress
/// stream so the client can see what the config asked for and whether it
/// matters on the chosen backend.
fn host_requirements_message(
    requirements: &HostRequirements,
    docker_host: &Host,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(cpus) = requirements.cpus {
        parts.push(format!("cpus={cpus}"));
    }
    if let Some(ref memory) = requirements.memory {
        parts.push(format!("memory={memory}"));
    }
    if let Some(ref storage) = requirements.storage {
        parts.push(format!("storage={storage}"));
    }
    if let Some(ref gpu) = requirements.gpu {
        match gpu {
            GpuRequirement::Required(true) => parts.push("gpu=required".to_string()),
            GpuRequirement::Required(false) => {}
            GpuRequirement::Optional(s) => parts.push(format!("gpu={s}")),
            GpuRequirement::Detailed(details) => parts.push(format!("gpu={details:?}")),
        }
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join(", ");
    let note = match docker_host {
        Host::Localhost | Host::Ssh { .. } => "advisory only on local/remote Docker",
        Host::Kubernetes { .. } => "mapped to pod resource requests on Kubernetes",
    };
    Some(format!("hostRequirements: {joined} ({note})"))
}

/// Verify that every file under each bind mount source is owned by the
/// current user.  The daemon will tar these directories and upload them
/// into the container via the pod server, which runs as a regular user
/// and cannot chown arbitrary files.
fn validate_bind_mount_ownership(devcontainer: &DevContainer) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let my_uid = nix::unistd::getuid().as_raw();
    for m in devcontainer.resolved_mounts()? {
        if m.mount_type != MountType::Bind {
            continue;
        }
        let source = match &m.source {
            Some(s) => PathBuf::from(s),
            None => continue,
        };
        if !source.exists() {
            let source = source.display();
            return Err(anyhow::anyhow!(
                "bind mount source '{source}' does not exist"
            ));
        }
        for entry in walkdir::WalkDir::new(&source) {
            let entry = entry.with_context(|| {
                let source = source.display();
                format!("walking bind mount source '{source}'")
            })?;
            let meta = entry.metadata().with_context(|| {
                let path = entry.path().display();
                format!("stat '{path}'")
            })?;
            if meta.uid() != my_uid {
                let path = entry.path().display();
                let owner = meta.uid();
                return Err(anyhow::anyhow!(
                    "bind mount source contains file '{path}' owned by uid {owner}, \
                     not the current user (uid {my_uid}). \
                     All files must be owned by the current user for remote bind mounts."
                ));
            }
        }
    }
    Ok(())
}

/// Resolve all devcontainer.json variables using the client-provided
/// `${localEnv:...}` map and the repo path.
///
/// Runs in two passes because `containerWorkspaceFolder` is derived from
/// `workspace_folder` after its own substitution pass, and everything
/// else can then reference the derived value.
fn resolve_devcontainer_vars(
    dc: DevContainer,
    repo_path: &Path,
    pod_name: &str,
    local_env: &HashMap<String, String>,
) -> DevContainer {
    let devcontainer_id = compute_devcontainer_id(repo_path, pod_name);

    // Canonicalize for ${localWorkspaceFolder} so symlinks like
    // /var -> /private/var on macOS resolve to the form Docker bind
    // mounts need.  The DB still keys pods by the client's original
    // repo_path so this is only used for substitution.
    let canonical = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());
    let local_ws = canonical
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    let local_ws_basename = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    // workspace_folder may itself contain ${devcontainerId} or
    // ${localWorkspaceFolderBasename}, so resolve it before deriving
    // containerWorkspaceFolder from it.
    let workspace_folder = dc.workspace_folder.as_ref().map(|wf| {
        substitute_vars(
            wf,
            &SubstitutionContext {
                local_env: Some(local_env.clone()),
                local_workspace_folder: Some(local_ws.clone()),
                local_workspace_folder_basename: Some(local_ws_basename.clone()),
                // containerWorkspaceFolder is derived from workspace_folder,
                // so it cannot be resolved yet.
                container_workspace_folder: None,
                container_workspace_folder_basename: None,
                devcontainer_id: Some(devcontainer_id.clone()),
            },
        )
    });
    let dc = DevContainer {
        workspace_folder,
        ..dc
    };

    let container_ws = dc.container_repo_path(repo_path);
    let container_ws_str = container_ws.to_string_lossy().to_string();
    let container_ws_basename = container_ws
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    dc.substitute(&SubstitutionContext {
        local_env: Some(local_env.clone()),
        local_workspace_folder: Some(local_ws),
        local_workspace_folder_basename: Some(local_ws_basename),
        container_workspace_folder: Some(container_ws_str),
        container_workspace_folder_basename: Some(container_ws_basename),
        devcontainer_id: Some(devcontainer_id),
    })
}

/// Clean up pod refs in a repo's git directory.
///
/// Removes all refs matching `refs/rumpelpod/*@<pod_name>` and the primary
/// shortcut `refs/rumpelpod/<pod_name>`.
fn cleanup_pod_refs_in_git_dir(git_dir: &Path, pod_name: &PodName) {
    let pod = &pod_name.0;
    let git_dir_arg = git_dir.to_string_lossy();
    let pattern = format!("refs/rumpelpod/*@{pod}");
    if let Ok(output) = Command::new("git")
        .args([
            "--git-dir",
            &git_dir_arg,
            "for-each-ref",
            "--format=%(refname)",
            &pattern,
        ])
        .output()
    {
        if output.status.success() {
            let refs = String::from_utf8_lossy(&output.stdout);
            for ref_name in refs.lines().filter(|s| !s.is_empty()) {
                let _ = Command::new("git")
                    .args(["--git-dir", &git_dir_arg, "update-ref", "-d", ref_name])
                    .output();
            }
        }
    }

    // Remove the primary shortcut ref.
    let alias_ref = format!("refs/rumpelpod/{pod}");
    let _ = Command::new("git")
        .args(["--git-dir", &git_dir_arg, "update-ref", "-d", &alias_ref])
        .output();
}

/// Clean up refs for a deleted pod.
///
/// Cleans up the repo .git dir and all submodule git dirs.
fn cleanup_pod_refs(repo_path: &Path, pod_name: &PodName) {
    let git_dir = repo_path.join(".git");
    cleanup_pod_refs_in_git_dir(&git_dir, pod_name);

    // Also clean up refs in each submodule git dir.
    for sub in gateway::detect_submodules(repo_path) {
        let sub_workdir = repo_path.join(&sub.displaypath);
        if let Ok(sub_git_dir) = gateway::resolve_git_dir(&sub_workdir) {
            if sub_git_dir.exists() {
                cleanup_pod_refs_in_git_dir(&sub_git_dir, pod_name);
            }
        }
    }
}

/// Parsed docker run arguments extracted from devcontainer.json `runArgs`.
///
/// These are mapped to the corresponding bollard `HostConfig` fields when
/// creating the container.  Values we don't recognise are silently ignored
/// (they likely belong to newer Docker versions or are unsupported).
struct DockerRunArgs {
    runtime: Option<String>,
    network: Option<String>,
    devices: Vec<String>,
    cap_add: Vec<String>,
    security_opt: Vec<String>,
    labels: Vec<(String, String)>,
    privileged: bool,
    init: bool,
}

/// Map devcontainer.json `runArgs` strings to bollard `HostConfig` fields.
///
/// Handles `--key=value` and `--key value` forms for all recognised flags.
fn parse_run_args_for_docker(args: &[String]) -> DockerRunArgs {
    let mut result = DockerRunArgs {
        runtime: None,
        network: None,
        devices: Vec::new(),
        cap_add: Vec::new(),
        security_opt: Vec::new(),
        labels: Vec::new(),
        privileged: false,
        init: false,
    };

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(val) = strip_flag(arg, "--runtime") {
            result.runtime = Some(val.to_string());
        } else if arg == "--runtime" {
            if let Some(val) = iter.next() {
                result.runtime = Some(val.to_string());
            }
        } else if let Some(val) = strip_flag(arg, "--network") {
            result.network = Some(val.to_string());
        } else if arg == "--network" {
            if let Some(val) = iter.next() {
                result.network = Some(val.to_string());
            }
        } else if let Some(val) = strip_flag(arg, "--device") {
            result.devices.push(val.to_string());
        } else if arg == "--device" {
            if let Some(val) = iter.next() {
                result.devices.push(val.to_string());
            }
        } else if let Some(val) = strip_flag(arg, "--cap-add") {
            result.cap_add.push(val.to_string());
        } else if arg == "--cap-add" {
            if let Some(val) = iter.next() {
                result.cap_add.push(val.to_string());
            }
        } else if let Some(val) = strip_flag(arg, "--security-opt") {
            result.security_opt.push(val.to_string());
        } else if arg == "--security-opt" {
            if let Some(val) = iter.next() {
                result.security_opt.push(val.to_string());
            }
        } else if let Some(val) = strip_flag(arg, "--label") {
            if let Some((k, v)) = val.split_once('=') {
                result.labels.push((k.to_string(), v.to_string()));
            }
        } else if arg == "--label" {
            if let Some(val) = iter.next() {
                if let Some((k, v)) = val.split_once('=') {
                    result.labels.push((k.to_string(), v.to_string()));
                }
            }
        } else if arg == "--privileged" {
            result.privileged = true;
        } else if arg == "--init" {
            result.init = true;
        }
    }

    result
}

/// Strip `--flag=` prefix and return the value, or None if the arg doesn't
/// start with `--flag=`.
fn strip_flag<'a>(arg: &'a str, flag: &str) -> Option<&'a str> {
    let prefix = format!("{flag}=");
    arg.strip_prefix(&prefix)
}

/// Merge an optional Vec from devcontainer.json first-class properties with
/// additional values parsed from runArgs.
fn merge_string_vecs(base: Option<&Vec<String>>, extra: &[String]) -> Option<Vec<String>> {
    let mut result: Vec<String> = base.cloned().unwrap_or_default();
    result.extend(extra.iter().cloned());
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// A bind mount source + container target pair, kept around so the daemon
/// can populate the mount after the container starts.
struct BindSource {
    /// Host-side path to the directory.
    source: PathBuf,
    /// Absolute path inside the container.
    target: String,
}

/// Inputs to `create_pod_container`, produced by either the fresh-pod
/// launch path or the fork-mode launch path.  Holds everything the
/// docker/k8s creation step needs after image resolution and gateway
/// setup are done.
struct ResolvedLaunch {
    pod_name: PodName,
    repo_path: PathBuf,
    docker_host: Host,
    devcontainer: DevContainer,
    raw_devcontainer_json: String,
    image: Image,
    image_built: bool,
    git_setup: GitSetupParams,
    local_env_vars: HashMap<String, String>,
    /// Docker mount list with bind mounts on remote hosts already
    /// converted to named volumes.  Unused by the k8s path, which
    /// builds its own mount spec.
    mounts: Vec<devcontainer::MountObject>,
    /// Host-side bind mount sources that get tar-uploaded into the
    /// container's volumes after startup.  Empty for local docker hosts.
    bind_sources: Vec<BindSource>,
    container_repo_path: PathBuf,
    executor: crate::executor::Executor,
    /// `Some` only for localhost Docker, where client-side interactive
    /// exec can reuse the exact daemon socket.  SSH and k8s clients
    /// connect through their native transports.
    docker_socket: Option<PathBuf>,
}

/// On remote hosts, convert bind mounts to named volumes so the container
/// can be created without referencing the developer's filesystem.  Returns
/// the (possibly converted) mount list and the original bind source/target
/// pairs that still need to be uploaded.
fn split_bind_mounts(
    mounts: Vec<devcontainer::MountObject>,
    host: &Host,
    pod_name: &str,
) -> (Vec<devcontainer::MountObject>, Vec<BindSource>) {
    if !host.is_remote() {
        return (mounts, Vec::new());
    }

    let mut converted = Vec::with_capacity(mounts.len());
    let mut bind_sources = Vec::new();

    for m in mounts {
        if m.mount_type != devcontainer::MountType::Bind {
            converted.push(m);
            continue;
        }

        let source_path = m.source.as_deref().unwrap_or("");
        bind_sources.push(BindSource {
            source: PathBuf::from(source_path),
            target: m.target.clone(),
        });

        // Deterministic volume name so re-creation reuses the same volume.
        let hash_input = format!("{pod_name}\0{}", m.target);
        let hash = Sha256::digest(hash_input.as_bytes());
        let short_hash = hex::encode(&hash[..6]);
        let volume_name = format!("rumpelpod-bind-{short_hash}");

        converted.push(devcontainer::MountObject {
            mount_type: devcontainer::MountType::Volume,
            source: Some(volume_name),
            target: m.target,
            // Writable so we can copy data in; original read_only is not
            // preserved (documented limitation for now).
            read_only: None,
        });
    }

    (converted, bind_sources)
}

/// Build a single tar of all bind mount sources (entries at absolute
/// container paths) and stream it to the pod server in one request.
fn upload_bind_mounts(pod: &PodClient, binds: &[BindSource]) -> Result<()> {
    if binds.is_empty() {
        return Ok(());
    }

    let (read_end, write_end) = std::io::pipe().context("creating pipe for bind mount tar")?;

    let binds_owned: Vec<(PathBuf, String)> = binds
        .iter()
        .map(|b| (b.source.clone(), b.target.clone()))
        .collect();

    let writer_thread = std::thread::spawn(move || -> Result<()> {
        let mut archive = tar::Builder::new(write_end);
        archive.follow_symlinks(true);
        for (source, target) in &binds_owned {
            // target is absolute, e.g. "/mnt/data".  Strip the leading
            // slash so tar entries are "mnt/data/...".
            let target_stripped = target.strip_prefix('/').unwrap_or(target);

            if source.is_dir() {
                for entry in walkdir::WalkDir::new(source) {
                    let entry = entry.with_context(|| {
                        let source = source.display();
                        format!("walking bind source '{source}'")
                    })?;
                    let rel = entry
                        .path()
                        .strip_prefix(source)
                        .expect("walkdir entry must be under source");
                    let tar_path = Path::new(target_stripped).join(rel);
                    let meta = entry.metadata().with_context(|| {
                        let path = entry.path().display();
                        format!("stat '{path}'")
                    })?;
                    if meta.is_dir() {
                        let mut header = tar::Header::new_gnu();
                        header.set_entry_type(tar::EntryType::Directory);
                        header.set_size(0);
                        header.set_mode(meta.permissions().mode());
                        header.set_cksum();
                        archive
                            .append_data(&mut header, &tar_path, std::io::empty())
                            .with_context(|| {
                                let tar_path = tar_path.display();
                                format!("adding directory '{tar_path}' to tar")
                            })?;
                    } else if meta.is_file() {
                        archive
                            .append_path_with_name(entry.path(), &tar_path)
                            .with_context(|| {
                                let tar_path = tar_path.display();
                                format!("adding file '{tar_path}' to tar")
                            })?;
                    }
                    // Skip symlinks, sockets, etc. for now.
                }
            } else {
                // Single file bind mount.
                let file_name = source
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let tar_path = Path::new(target_stripped).join(&file_name);
                archive
                    .append_path_with_name(source, &tar_path)
                    .with_context(|| {
                        let source = source.display();
                        format!("adding file '{source}' to tar")
                    })?;
            }
        }
        archive.into_inner().context("finalizing bind mount tar")?;
        Ok(())
    });

    pod.init_mounts(read_end)
        .context("uploading bind mount data to container")?;

    writer_thread
        .join()
        .expect("bind mount tar writer panicked")?;

    Ok(())
}

/// Build a [`crate::executor::PodSpec`] for kubernetes from devcontainer
/// config.  Bind-mount sources surface through `bind_sources` so the
/// caller can upload the data once the pod is running.  Warnings for
/// k8s-incompatible devcontainer knobs are emitted via `progress_tx`.
#[allow(clippy::too_many_arguments)]
fn build_k8s_pod_spec(
    pod_name: &PodName,
    image: &str,
    repo_path: &Path,
    dc: &DevContainer,
    node_selector: Option<std::collections::BTreeMap<String, String>>,
    tolerations: Option<Vec<crate::config::KubernetesToleration>>,
    progress_tx: &std::sync::mpsc::Sender<OutputLine>,
    bind_sources: &mut Vec<BindSource>,
) -> Result<crate::executor::PodSpec> {
    use crate::executor::{DockerOnly, K8sOnly, Mount, PodSpec, Resources};

    let all_mounts = dc.resolved_mounts()?;
    check_no_unresolved_mount_vars(&all_mounts)?;
    let run_args = dc.run_args.as_deref().unwrap_or(&[]);
    let run_args_config = parse_run_args_for_docker(run_args);

    let spec_mounts: Vec<Mount> = all_mounts
        .iter()
        .map(|m| match m.mount_type {
            devcontainer::MountType::Volume => Mount {
                source: None,
                target: m.target.clone(),
                mount_type: crate::executor::MountType::Volume,
                read_only: m.read_only.unwrap_or(false),
            },
            devcontainer::MountType::Tmpfs => Mount {
                source: None,
                target: m.target.clone(),
                mount_type: crate::executor::MountType::Tmpfs,
                read_only: m.read_only.unwrap_or(false),
            },
            devcontainer::MountType::Bind => {
                bind_sources.push(BindSource {
                    source: PathBuf::from(m.source.as_deref().unwrap_or("")),
                    target: m.target.clone(),
                });
                // K8s has no analogue of docker bind mounts; fall back
                // to a disk-backed emptyDir that the daemon tars the
                // host data into after the pod starts.
                Mount {
                    source: None,
                    target: m.target.clone(),
                    mount_type: crate::executor::MountType::Bind,
                    read_only: false,
                }
            }
        })
        .collect();

    let privileged = dc.privileged == Some(true) || run_args_config.privileged;
    let cap_add =
        merge_string_vecs(dc.cap_add.as_ref(), &run_args_config.cap_add).unwrap_or_default();

    let merged_security_opt =
        merge_string_vecs(dc.security_opt.as_ref(), &run_args_config.security_opt)
            .unwrap_or_default();
    let mut seccomp_unconfined = false;
    let mut apparmor_unconfined = false;
    for opt in &merged_security_opt {
        if opt == "seccomp=unconfined" || opt == "seccomp:unconfined" {
            seccomp_unconfined = true;
        } else if opt == "apparmor=unconfined" || opt == "apparmor:unconfined" {
            apparmor_unconfined = true;
        } else {
            log::warn!("unrecognized security_opt '{opt}' ignored on Kubernetes");
        }
    }

    let resources = dc.host_requirements.as_ref().and_then(|hr| {
        let cpu = hr.cpus.map(|c| c.to_string());
        let memory = hr
            .memory
            .as_deref()
            .and_then(crate::k8s::convert_memory_to_k8s);
        if cpu.is_some() || memory.is_some() {
            Some(Resources { cpu, memory })
        } else {
            None
        }
    });

    if dc.init == Some(true) || run_args_config.init {
        progress_tx
            .send(OutputLine::Stderr(
                "warning: init has no effect on Kubernetes -- \
                 bake an init process (e.g. tini) into the image instead"
                    .into(),
            ))
            .ok();
    }
    if !run_args_config.devices.is_empty() {
        progress_tx
            .send(OutputLine::Stderr(
                "warning: --device has no effect on Kubernetes -- \
                 use cluster-specific device plugins instead"
                    .into(),
            ))
            .ok();
    }
    if run_args_config.network.is_some() {
        progress_tx
            .send(OutputLine::Stderr(
                "warning: --network has no effect on Kubernetes".into(),
            ))
            .ok();
    }
    if !run_args_config.labels.is_empty() {
        progress_tx
            .send(OutputLine::Stderr(
                "warning: --label in runArgs has no effect on Kubernetes".into(),
            ))
            .ok();
    }

    let env: Vec<(String, String)> = dc
        .container_env
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();

    let override_command = dc.override_command.unwrap_or(true);
    let cmd = if override_command {
        Some(vec!["sleep".to_string(), "infinity".to_string()])
    } else {
        None
    };

    let labels = crate::executor::k8s_pod_labels(pod_name, repo_path);
    let annotations = crate::executor::k8s_pod_annotations();

    // "runc" is the Docker default runtime; on k8s, omitting
    // runtimeClassName achieves the same thing.  Only set it for
    // non-default runtimes (e.g. sysbox-runc) that need a matching
    // RuntimeClass object in the cluster.  Per-pod devcontainer
    // runArgs win over the environment default
    // (`RUMPELPOD_RUNTIME_CLASS`), which xtest uses to thread its
    // `--runtime <name>` flag through to every pod without touching
    // the on-disk rumpelpod config format.
    let runtime = run_args_config
        .runtime
        .or_else(|| std::env::var("RUMPELPOD_RUNTIME_CLASS").ok());

    Ok(PodSpec {
        image: image.to_string(),
        hostname: crate::executor::hostname_for(pod_name),
        cmd,
        env,
        mounts: spec_mounts,
        labels,
        annotations,
        privileged,
        cap_add,
        seccomp_unconfined,
        apparmor_unconfined,
        resources,
        runtime,
        docker_only: DockerOnly::default(),
        k8s_only: K8sOnly {
            node_selector,
            tolerations,
        },
    })
}

/// Build a [`crate::executor::PodSpec`] for docker from devcontainer
/// config.  The executor's docker backend consumes this directly.
#[allow(clippy::too_many_arguments)]
fn build_docker_pod_spec(
    pod_name: &PodName,
    image: &Image,
    repo_path: &Path,
    container_repo_path: &Path,
    dc: &DevContainer,
    mounts: &[devcontainer::MountObject],
    publish_ports: &HashMap<u16, u16>,
) -> Result<crate::executor::PodSpec> {
    use crate::executor::{DockerOnly, K8sOnly, Mount, PodSpec};

    let run_args = dc.run_args.as_deref().unwrap_or(&[]);
    let run_args_config = parse_run_args_for_docker(run_args);

    let mut labels = crate::executor::docker_pod_labels(pod_name, repo_path, container_repo_path);
    for (k, v) in &run_args_config.labels {
        labels.insert(k.clone(), v.clone());
    }

    let mut env: Vec<(String, String)> = dc
        .container_env
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    env.push((
        "SSH_AUTH_SOCK".to_string(),
        crate::pod::SSH_AGENT_SOCK_PATH.to_string(),
    ));

    let spec_mounts: Vec<Mount> = mounts
        .iter()
        .map(|m| {
            let mount_type = match m.mount_type {
                devcontainer::MountType::Bind => crate::executor::MountType::Bind,
                devcontainer::MountType::Volume => crate::executor::MountType::Volume,
                devcontainer::MountType::Tmpfs => crate::executor::MountType::Tmpfs,
            };
            Mount {
                source: m.source.clone(),
                target: m.target.clone(),
                mount_type,
                read_only: m.read_only.unwrap_or(false),
            }
        })
        .collect();

    let privileged = dc.privileged == Some(true) || run_args_config.privileged;
    let cap_add =
        merge_string_vecs(dc.cap_add.as_ref(), &run_args_config.cap_add).unwrap_or_default();
    let security_opt = merge_string_vecs(dc.security_opt.as_ref(), &run_args_config.security_opt)
        .unwrap_or_default();

    let override_command = dc.override_command.unwrap_or(true);
    let cmd = if override_command {
        Some(vec!["sleep".to_string(), "infinity".to_string()])
    } else {
        None
    };

    let init = dc.init == Some(true) || run_args_config.init;

    Ok(PodSpec {
        image: image.0.clone(),
        hostname: crate::executor::Hostname::new(sanitize_hostname(&pod_name.0))
            .expect("sanitized hostname is DNS-1123"),
        cmd,
        env,
        mounts: spec_mounts,
        labels,
        annotations: std::collections::BTreeMap::new(),
        privileged,
        cap_add,
        // Raw security_opt passthrough for docker only.  The executor's
        // docker path re-materializes `seccomp=unconfined` /
        // `apparmor=unconfined` from the semantic booleans; anything
        // else flows through docker_only.security_opt unchanged.
        seccomp_unconfined: false,
        apparmor_unconfined: false,
        resources: None,
        runtime: run_args_config.runtime,
        docker_only: DockerOnly {
            init,
            devices: run_args_config.devices,
            network: run_args_config.network,
            security_opt,
            port_bindings: publish_ports.clone(),
        },
        k8s_only: K8sOnly::default(),
    })
}

/// Check if an error looks like the known Docker overlay2 issue where the
/// container filesystem is not fully visible right after creation.
fn is_overlay2_setup_error(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err);
    // "Directory nonexistent" comes from git clone, "index.lock" from
    // git reset/clean -- both are overlay2 failing to expose .git.
    if (msg.contains("Directory nonexistent") && msg.contains(".git"))
        || (msg.contains("index.lock") && msg.contains("No such file or directory"))
    {
        return true;
    }
    // copy_rumpel_binary runs before git ops and hits the same overlay2
    // issue -- the container filesystem is not yet writeable/visible.
    if msg.contains("creating /opt/rumpelpod/bin")
        || msg.contains("writing rumpel binary")
        || msg.contains("making rumpel binary executable")
    {
        return true;
    }
    false
}

/// Parse a Port spec into the numeric container port.
fn resolve_port_number(port: &Port) -> Option<u16> {
    match port {
        Port::Number(n) => Some(*n),
        Port::String(s) => {
            if let Some((_host, container)) = s.split_once(':') {
                container.parse().ok()
            } else {
                s.parse().ok()
            }
        }
    }
}

/// Set up exec-proxy listeners on the host for each devcontainer
/// `forwardPorts` entry.
///
/// On fresh pod creation (no DB rows for this pod) we bind a host
/// port near the container port, insert a row, and start one
/// `rumpel tcp-proxy` exec per accepted connection.
///
/// On reconnect (rows exist) the DB is authoritative: we iterate the
/// saved rows, try to reclaim the same host port to keep URLs stable
/// across daemon restarts, and fall back to a port near the
/// container port if the saved one has been grabbed by another
/// process.
///
/// Returns the listener handles; the caller must keep them alive
/// (dropping cancels the listener).
fn setup_port_forwarding(
    conn: &Connection,
    executor: &crate::executor::Executor,
    pod_id: &crate::executor::PodId,
    db_pod_id: db::PodId,
    forward_ports: &[Port],
    ports_attributes: &std::collections::HashMap<String, PortAttributes>,
    other_ports_attributes: &Option<PortAttributes>,
) -> Result<Vec<crate::exec_proxy::ExecProxyHandle>> {
    let existing = db::list_forwarded_ports(conn, db_pod_id)?;
    let allocated_globally: std::collections::HashSet<u16> =
        db::get_all_allocated_local_ports(conn)?
            .into_iter()
            .collect();

    let mut handles = Vec::new();

    if existing.is_empty() {
        let mut reserved = allocated_globally;
        for port_spec in forward_ports {
            let container_port = match resolve_port_number(port_spec) {
                Some(p) => p,
                None => {
                    log::warn!("skipping invalid port spec: {port_spec:?}");
                    continue;
                }
            };

            let listener = block_on(bind_near(container_port, &reserved))
                .context("binding host listener for forwardPorts entry")?;
            let local_port = listener.local_addr()?.port();
            reserved.insert(local_port);

            let label = ports_attributes
                .get(&container_port.to_string())
                .or(other_ports_attributes.as_ref())
                .and_then(|a| a.label.as_deref())
                .unwrap_or("")
                .to_string();

            db::insert_forwarded_port(conn, db_pod_id, container_port, local_port, &label)?;

            let handle = crate::exec_proxy::start_exec_proxy_on_listener(
                listener,
                executor.clone(),
                pod_id.clone(),
                container_port,
            )?;
            handles.push(handle);
        }
    } else {
        let my_existing_ports: std::collections::HashSet<u16> =
            existing.iter().map(|p| p.local_port).collect();
        let mut reserved: std::collections::HashSet<u16> = allocated_globally
            .difference(&my_existing_ports)
            .copied()
            .collect();

        for saved in existing {
            let listener = block_on(rebind_or_near(
                saved.local_port,
                saved.container_port,
                &reserved,
            ))
            .context("re-binding host listener on reconnect")?;
            let actual = listener.local_addr()?.port();
            if actual != saved.local_port {
                conn.execute(
                    "UPDATE forwarded_ports SET local_port = ? \
                     WHERE pod_id = ? AND container_port = ?",
                    rusqlite::params![actual, i64::from(db_pod_id), saved.container_port],
                )
                .context("updating forwarded_ports.local_port after rebind")?;
            }
            reserved.insert(actual);

            let handle = crate::exec_proxy::start_exec_proxy_on_listener(
                listener,
                executor.clone(),
                pod_id.clone(),
                saved.container_port,
            )?;
            handles.push(handle);
        }
    }

    Ok(handles)
}

/// Bind a loopback TCP listener, preferring a port near `preferred`.
///
/// Tries `preferred`, `preferred+1`, ..., up to `NEAR_ATTEMPTS`
/// consecutive ports; falls back to an OS-assigned port if none of
/// those are free.  Ports in `reserved` are skipped without an
/// actual bind attempt -- used so rows being processed in the same
/// pass don't race each other.
async fn bind_near(
    preferred: u16,
    reserved: &std::collections::HashSet<u16>,
) -> Result<tokio::net::TcpListener> {
    const NEAR_ATTEMPTS: u16 = 10;
    for offset in 0..NEAR_ATTEMPTS {
        let port = match preferred.checked_add(offset) {
            Some(p) if p != 0 => p,
            _ => continue,
        };
        if reserved.contains(&port) {
            continue;
        }
        if let Ok(l) = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
            return Ok(l);
        }
    }
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding loopback listener with OS-assigned port")
}

/// Like [`bind_near`] but first tries `preferred` exactly.
///
/// Used on reconnect: the daemon wants to reclaim the host port it
/// used last time so client URLs stay stable, but another process
/// may have grabbed it while the daemon was down -- in which case we
/// fall back to a port near the container port.
async fn rebind_or_near(
    preferred: u16,
    fallback_near: u16,
    reserved: &std::collections::HashSet<u16>,
) -> Result<tokio::net::TcpListener> {
    if !reserved.contains(&preferred) && preferred != 0 {
        if let Ok(l) = tokio::net::TcpListener::bind(format!("127.0.0.1:{preferred}")).await {
            return Ok(l);
        }
    }
    bind_near(fallback_near, reserved).await
}

struct PodGitInfo {
    /// e.g. "ahead 2, behind 3" or "up to date"
    repo_state: String,
    /// Committer timestamp (unix seconds) of the tip of the pod's primary branch.
    last_commit_time: i64,
}

enum ReconnectPodResult {
    Connected(Box<LaunchResult>),
    Gone(anyhow::Error),
    Unavailable(anyhow::Error),
}

/// Compute the git status of the pod's primary branch vs the currently checked out commit.
/// Returns None if the ref doesn't exist.
/// "ahead N" means the pod is N commits ahead of the host HEAD.
fn compute_git_info(repo_path: &Path, pod_name: &str) -> Option<PodGitInfo> {
    use git2::Repository;

    let repo = Repository::open(repo_path).ok()?;

    // Get the current HEAD commit (host)
    let head = repo.head().ok()?;
    let host_oid = head.target()?;

    // Get the pod's primary branch ref: refs/rumpelpod/<pod_name>
    let remote_ref_name = format!("refs/rumpelpod/{pod_name}");
    let remote_ref = repo.find_reference(&remote_ref_name).ok()?;
    let pod_oid = remote_ref.target()?;

    let commit = repo.find_commit(pod_oid).ok()?;
    let last_commit_time = commit.committer().when().seconds();

    let repo_state = if host_oid == pod_oid {
        "up to date".to_string()
    } else {
        // Count ahead/behind from pod's perspective
        // (ahead, behind) = how many commits pod is ahead/behind host
        let (ahead, behind) = repo.graph_ahead_behind(pod_oid, host_oid).ok()?;

        match (ahead, behind) {
            (0, 0) => "up to date".to_string(),
            (a, 0) => format!("ahead {a}"),
            (0, b) => format!("behind {b}"),
            (a, b) => format!("ahead {a}, behind {b}"),
        }
    };

    Some(PodGitInfo {
        repo_state,
        last_commit_time,
    })
}

/// Copy only the minimal Claude Code config files needed to authenticate
/// and run inside a container.  Avoids leaking conversation history,
/// telemetry, stats, and other projects' data into untrusted pods.
///
/// Builds a tar in a pipe-thread containing the transformed
/// .claude.json (whitelisted keys + per-project remap), the credentials
/// file, settings.json (raw -- statusline/hooks are injected by the
/// pod's PUT handler), the project-data dir remapped to the container
/// path, and the filtered history.jsonl.  Streams it via PUT
/// /agent-files/claude.
fn copy_claude_config_via_pod(
    pod: &PodClient,
    repo_path: &Path,
    container_repo_path: &Path,
    permission_hook: bool,
    copy_sessions: bool,
) -> Result<()> {
    let local_home = dirs::home_dir().context("Could not determine home directory")?;
    let claude_dir = local_home.join(".claude");

    // Materialize all in-memory pieces up-front so the pipe-thread sees
    // owned data and the host I/O errors surface synchronously here.
    let claude_json = match std::fs::read(local_home.join(".claude.json")) {
        Ok(data) => Some(strip_claude_json(&data, repo_path, container_repo_path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::from(e).context("reading ~/.claude.json")),
    };
    let credentials = match std::fs::read(claude_dir.join(".credentials.json")) {
        Ok(data) => Some(data),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::from(e).context("reading ~/.claude/.credentials.json")),
    };
    let settings = match std::fs::read(claude_dir.join("settings.json")) {
        Ok(data) => Some(data),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::from(e).context("reading ~/.claude/settings.json")),
    };
    let history = std::fs::read(local_home.join(".claude/history.jsonl"))
        .ok()
        .map(|raw| filter_history(&raw, repo_path, container_repo_path))
        .filter(|f| !f.is_empty());

    let container_dir_name = claude_project_dir_name(container_repo_path);
    let project_dir = if copy_sessions {
        let local_dir_name = claude_project_dir_name(repo_path);
        let local_project_dir = local_home.join(".claude/projects").join(&local_dir_name);
        local_project_dir.is_dir().then_some(local_project_dir)
    } else {
        None
    };

    let (read_end, write_end) = std::io::pipe().context("creating pipe for claude tar")?;
    let handle = std::thread::spawn(move || -> Result<()> {
        let mut archive = tar::Builder::new(write_end);
        if let Some(data) = claude_json {
            append_bytes(&mut archive, ".claude.json", &data)?;
        }
        if let Some(data) = credentials {
            append_bytes(&mut archive, ".claude/.credentials.json", &data)?;
        }
        if let Some(data) = settings {
            append_bytes(&mut archive, ".claude/settings.json", &data)?;
        }
        if let Some(data) = history {
            append_bytes(&mut archive, ".claude/history.jsonl", &data)?;
        }
        if let Some(dir) = project_dir {
            let dest = format!(".claude/projects/{container_dir_name}");
            archive
                .append_dir_all(&dest, &dir)
                .with_context(|| format!("archiving {dest}"))?;
        }
        archive.into_inner().context("finalizing claude tar")?;
        Ok(())
    });

    pod.put_agent_files("claude", read_end, Some(permission_hook))
        .context("uploading claude config")?;
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("claude tar thread panicked"))??;

    Ok(())
}

/// Copy the local machine's pi auth/config into the pod via
/// /agent-files/pi.
///
/// Streams ~/.pi/agent/{auth.json,settings.json,models.json} (each only
/// if present).  When `trust_workspace` is set, `defaultProjectTrust`
/// is forced to "always" in the copied settings.json so pi's in-pod TUI
/// never blocks on the project-trust prompt -- the pod is the sandbox.
fn copy_pi_config_via_pod(pod: &PodClient, trust_workspace: bool) -> Result<()> {
    let local_home = dirs::home_dir().context("Could not determine home directory")?;
    let agent_dir = local_home.join(".pi/agent");

    // Materialize all pieces up-front so the pipe-thread sees owned data
    // and host I/O errors surface synchronously here.
    let auth = read_optional(&agent_dir.join("auth.json"), "~/.pi/agent/auth.json")?;
    let models = read_optional(&agent_dir.join("models.json"), "~/.pi/agent/models.json")?;
    let settings = {
        let raw = read_optional(
            &agent_dir.join("settings.json"),
            "~/.pi/agent/settings.json",
        )?;
        if trust_workspace {
            Some(force_pi_trust(raw.as_deref())?)
        } else {
            raw
        }
    };

    let (read_end, write_end) = std::io::pipe().context("creating pipe for pi tar")?;
    let handle = std::thread::spawn(move || -> Result<()> {
        let mut archive = tar::Builder::new(write_end);
        if let Some(data) = auth {
            append_bytes(&mut archive, ".pi/agent/auth.json", &data)?;
        }
        if let Some(data) = settings {
            append_bytes(&mut archive, ".pi/agent/settings.json", &data)?;
        }
        if let Some(data) = models {
            append_bytes(&mut archive, ".pi/agent/models.json", &data)?;
        }
        archive.into_inner().context("finalizing pi tar")?;
        Ok(())
    });

    pod.put_agent_files("pi", read_end, None)
        .context("uploading pi config")?;
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("pi tar thread panicked"))??;

    Ok(())
}

/// Read a file, mapping a missing file to None.  Any other I/O error is
/// surfaced with the human-readable `label` for context.
fn read_optional(path: &Path, label: &str) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {label}"))),
    }
}

/// Force `defaultProjectTrust: "always"` into pi's settings.json,
/// starting from an empty object when the host has no settings file.
fn force_pi_trust(raw: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut obj: serde_json::Map<String, serde_json::Value> = match raw {
        Some(data) => serde_json::from_slice(data).context("parsing ~/.pi/agent/settings.json")?,
        None => serde_json::Map::new(),
    };
    obj.insert(
        "defaultProjectTrust".to_string(),
        serde_json::Value::String("always".to_string()),
    );
    serde_json::to_vec_pretty(&obj).context("serializing pi settings.json")
}

/// Append a byte slice as a regular file entry.  The tar crate's
/// `append_data` API needs an owned-style mutable header, so wrap the
/// bytes once here rather than at every call site.
fn append_bytes<W: std::io::Write>(
    archive: &mut tar::Builder<W>,
    rel: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, rel, data)
        .with_context(|| format!("archiving {rel}"))
}

/// Filter history.jsonl to entries matching this project, rewriting the
/// project path from the local machine to the container.
fn filter_history(data: &[u8], repo_path: &Path, container_repo_path: &Path) -> Vec<u8> {
    let local_project = repo_path.to_string_lossy();
    let container_project = container_repo_path.to_string_lossy();

    let mut filtered = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(mut obj) =
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(line)
        else {
            continue;
        };
        let matches = obj
            .get("project")
            .and_then(|v| v.as_str())
            .is_some_and(|p| p == &*local_project);
        if !matches {
            continue;
        }
        obj.insert(
            "project".to_string(),
            serde_json::Value::String(container_project.to_string()),
        );
        if let Ok(serialized) = serde_json::to_vec(&obj) {
            filtered.extend_from_slice(&serialized);
            filtered.push(b'\n');
        }
    }
    filtered
}

/// Claude stores per-project data under ~/.claude/projects/<dir-name> where
/// <dir-name> is the absolute path with every non-alphanumeric character
/// replaced by `-`.  This matches Claude Code's JS implementation:
///   path.replace(/[^a-zA-Z0-9]/g, "-")
fn claude_project_dir_name(repo_path: &Path) -> String {
    repo_path
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Keep only whitelisted keys from ~/.claude.json and remap the per-project
/// entry for `repo_path` so it appears under `container_repo_path`.
fn strip_claude_json(data: &[u8], repo_path: &Path, container_repo_path: &Path) -> Vec<u8> {
    // Global keys that are safe to copy into the container. Keys that leak
    // host filesystem paths (projects, githubRepoPaths) are omitted;
    // projects is remapped separately below.
    const KEEP_KEYS: &[&str] = &[
        "anonymousId",
        "autoUpdates",
        "autoUpdatesProtectedForNative",
        "birthdayHatAnimationCount",
        "bypassPermissionsModeAccepted",
        "cachedChromeExtensionInstalled",
        "cachedDynamicConfigs",
        "cachedExtraUsageDisabledReason",
        "cachedGrowthBookFeatures",
        "cachedStatsigGates",
        "changelogLastFetched",
        "claudeCodeFirstTokenDate",
        "clientDataCache",
        "customApiKeyResponses",
        "effortCalloutDismissed",
        "effortCalloutV2Dismissed",
        "feedbackSurveyState",
        "firstStartTime",
        "fullscreenUpsellSeenCount",
        "groveConfigCache",
        "hasAcknowledgedCostThreshold",
        "hasCompletedOnboarding",
        "hasSeenTasksHint",
        "hasShownOpus45Notice",
        "hasShownOpus46Notice",
        "hasVisitedExtraUsage",
        "hasVisitedPasses",
        "installMethod",
        "lastOnboardingVersion",
        "lastReleaseNotesSeen",
        "lspRecommendationDisabled",
        "numStartups",
        "oauthAccount",
        "officialMarketplaceAutoInstallAttempted",
        "officialMarketplaceAutoInstalled",
        "opus45MigrationComplete",
        "opus46FeedSeenCount",
        "opusProMigrationComplete",
        "passesEligibilityCache",
        "passesLastSeenCampaign",
        "passesLastSeenRemaining",
        "passesUpsellSeenCount",
        "penguinModeOrgEnabled",
        "primaryApiKey",
        "promptQueueUseCount",
        "s1mAccessCache",
        "s1mNonSubscriberAccessCache",
        "sonnet1m45MigrationComplete",
        "sonnet45MigrationComplete",
        "sonnet45MigrationTimestamp",
        "subscriptionNoticeCount",
        "thinkingMigrationComplete",
        "tipsHistory",
        "toolUsage",
        "userID",
    ];

    let Ok(mut obj) = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(data)
    else {
        // If the file is not valid JSON, pass through an empty object so
        // claude does not complain about a missing file.
        return b"{}".to_vec();
    };

    // Extract the project entry for this repo before stripping.
    let project_entry = obj
        .get("projects")
        .and_then(|v| v.as_object())
        .and_then(|projects| projects.get(&*repo_path.to_string_lossy()).cloned());

    obj.retain(|k, _| KEEP_KEYS.contains(&k.as_str()));

    // Build the project entry for the container workspace.  Start from the
    // host entry (if any) so per-project settings carry over, then force
    // trust-dialog and project-onboarding to accepted -- showing these
    // inside an already-sandboxed container is pointless friction.
    let container_key = container_repo_path.to_string_lossy().to_string();
    let mut entry = project_entry
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    entry.insert(
        "hasTrustDialogAccepted".to_string(),
        serde_json::Value::Bool(true),
    );
    entry.insert(
        "hasCompletedProjectOnboarding".to_string(),
        serde_json::Value::Bool(true),
    );
    let mut projects = serde_json::Map::new();
    projects.insert(container_key, serde_json::Value::Object(entry));
    obj.insert("projects".to_string(), serde_json::Value::Object(projects));

    serde_json::to_vec_pretty(&obj).unwrap_or_else(|_| b"{}".to_vec())
}

pub(crate) const RUMPEL_CONTAINER_BIN: &str = "/opt/rumpelpod/bin/rumpel";
pub(crate) const CLAUDE_CONTAINER_BIN: &str = "/opt/rumpelpod/bin/claude";
pub(crate) const CODEX_CONTAINER_BIN: &str = "/opt/rumpelpod/bin/codex";
pub(crate) const PI_CONTAINER_BIN: &str = "/opt/rumpelpod/bin/pi";
pub(crate) const GROK_CONTAINER_BIN: &str = "/opt/rumpelpod/bin/grok";

struct CodexProxyHandle {
    port: u16,
    token: String,
    _cancel_tx: tokio::sync::watch::Sender<bool>,
}

pub(crate) struct CodexProxyEndpoint {
    pub(crate) port: u16,
    pub(crate) token: String,
}

/// Enables test-only paths in container-serve.  Detected via the
/// same env var tests already set to enable the LLM cache proxy.
fn is_test_mode() -> bool {
    std::env::var("RUMPELPOD_TEST_LLM_OFFLINE").is_ok()
}

/// Read the server-port file that `rumpel container-serve` writes
/// after binding its TCP listener.  Polls from outside the pod via
/// exec since the file lives inside the container's FS.
fn read_container_server_port(
    executor: &crate::executor::Executor,
    pod_id: &crate::executor::PodId,
) -> Result<u16> {
    // In practice the file appears within a few hundred milliseconds
    // of the container-serve exec.
    let script = indoc::indoc! {r#"
        i=0
        while [ $i -lt 200 ]; do
            if [ -s /opt/rumpelpod/server-port ]; then
                cat /opt/rumpelpod/server-port
                exit 0
            fi
            i=$((i+1))
            sleep 0.1
        done
        echo "timeout waiting for /opt/rumpelpod/server-port" >&2
        exit 1
    "#};
    let out = executor
        .exec(
            pod_id,
            crate::executor::ExecRequest {
                cmd: vec!["sh".into(), "-c".into(), script.into()],
                workdir: None,
                env: Vec::new(),
                stdin: None,
            },
        )
        .context("waiting for server-port file in container")?;
    if out.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow::anyhow!(
            "server-port probe exited with {}: {}",
            out.exit_code,
            stderr.trim()
        ));
    }
    let text = std::str::from_utf8(&out.stdout)
        .context("server-port file contained non-utf8")?
        .trim();
    text.parse::<u16>()
        .with_context(|| format!("parsing server-port file content: {text:?}"))
}

/// Start the in-container HTTP server via detached docker exec and return
/// the bearer token for authenticating subsequent requests.
///
/// If the server is already running (token file exists and /events
/// responds), returns the existing token without starting a new process.
/// Start the in-container HTTP server via detached exec.
///
/// container-serve runs as the image USER -- the executor has no
/// override -- and calls switch_user() internally if it needs to drop
/// root.  Both backends treat this as fire-and-forget; readiness is
/// observed later by `PodClient::wait_and_connect` polling `/events`
/// through the exec proxy.
#[allow(clippy::too_many_arguments)]
fn start_container_server(
    executor: &crate::executor::Executor,
    pod_id: &crate::executor::PodId,
    container_repo_path: &Path,
    pod_name: &str,
    local_env_vars: &HashMap<String, String>,
    token: &str,
    git_setup: Option<&GitSetupParams>,
) -> Result<()> {
    let repo_path_str = container_repo_path.to_string_lossy().to_string();
    let mut cmd = vec![
        RUMPEL_CONTAINER_BIN.to_string(),
        "container-serve".to_string(),
        "--token".to_string(),
        token.to_string(),
        "--repo-path".to_string(),
        repo_path_str,
        "--pod-name".to_string(),
        pod_name.to_string(),
    ];
    if is_test_mode() {
        cmd.push("--test-mode".to_string());
    }
    for (key, value) in local_env_vars {
        cmd.push("--local-env".to_string());
        cmd.push(format!("{key}={value}"));
    }
    if let Some(setup) = git_setup {
        let json = serde_json::to_string(setup).context("serializing git_setup_spec")?;
        cmd.push("--git-setup-spec".to_string());
        cmd.push(json);
    }

    executor
        .exec_detached(
            pod_id,
            crate::executor::ExecRequest {
                cmd,
                workdir: None,
                env: Vec::new(),
                stdin: None,
            },
        )
        .context("starting container-serve")?;
    Ok(())
}

/// Progress handle returned by `DaemonServer::launch_pod` / `recreate_pod`.
///
/// The actual work runs on a background thread that sends build-output lines
/// through a channel.  `Iterator::next()` reads lines; `finish()` joins the
/// thread and returns the final result.
pub struct ServerLaunchProgress {
    rx: Option<std::sync::mpsc::Receiver<crate::image::OutputLine>>,
    handle: Option<std::thread::JoinHandle<Result<LaunchResult>>>,
}

impl Iterator for ServerLaunchProgress {
    type Item = crate::image::OutputLine;

    fn next(&mut self) -> Option<crate::image::OutputLine> {
        self.rx.as_ref()?.recv().ok()
    }
}

impl protocol::LaunchProgress for ServerLaunchProgress {
    fn finish(mut self) -> Result<LaunchResult> {
        // Drop the receiver so the background thread sees a closed channel
        // and stops sending.
        drop(self.rx.take());
        self.handle
            .take()
            .expect("finish() called twice on ServerLaunchProgress")
            .join()
            .map_err(|e| {
                let msg = e
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("(no message)");
                anyhow::anyhow!("launch thread panicked: {msg}")
            })?
    }
}

impl DaemonServer {
    /// Build an executor for `host`, ensuring the underlying host
    /// connection is up.  This is the single chokepoint by which the
    /// daemon talks to a backend; it implicitly registers the host
    /// in the registry on first use.
    fn host_executor(&self, host: &Host) -> Result<crate::executor::Executor> {
        let conn = self.host_connections.get_or_create(host)?;
        crate::executor::Executor::new(&conn)
    }

    /// Reconnect to an existing pod that is recorded in the database.
    ///
    /// Only a backend-confirmed Gone status means the DB row is stale.
    /// Transport failures are preserved so temporary host outages do not
    /// make an existing pod disappear from `rumpel list`.
    #[allow(clippy::too_many_arguments)]
    fn reconnect_pod(
        &self,
        pod_name: &PodName,
        repo_path: &Path,
        docker_host: &Host,
        devcontainer: &DevContainer,
        local_env_vars: &HashMap<String, String>,
        record: &db::PodRecord,
    ) -> ReconnectPodResult {
        if let Err(e) = gateway::install_host_hooks(repo_path) {
            return ReconnectPodResult::Unavailable(e);
        }

        let container_repo_path = devcontainer.container_repo_path(repo_path);
        match docker_host {
            Host::Kubernetes {
                context, namespace, ..
            } => {
                let pod_id = crate::executor::pod_id_for(pod_name, repo_path);
                let executor = match self.host_executor(docker_host) {
                    Ok(executor) => executor,
                    Err(e) => return ReconnectPodResult::Unavailable(e),
                };
                let status = match executor.status(&pod_id) {
                    Ok(status) => status,
                    Err(e) => return ReconnectPodResult::Unavailable(e),
                };
                match status {
                    PodStatus::Running => {}
                    PodStatus::Gone => {
                        return ReconnectPodResult::Gone(anyhow::anyhow!(
                            "k8s pod '{pod_id}' not found"
                        ));
                    }
                    PodStatus::Stopped
                    | PodStatus::Disconnected
                    | PodStatus::Stopping
                    | PodStatus::Deleting
                    | PodStatus::Broken => {
                        return ReconnectPodResult::Unavailable(anyhow::anyhow!(
                            "k8s pod is not running (status: {status:?})"
                        ));
                    }
                }
                let result = self.reconnect_k8s(
                    pod_name,
                    repo_path,
                    docker_host,
                    context,
                    namespace,
                    &executor,
                    &pod_id,
                    &container_repo_path,
                    record,
                );
                let result = match result {
                    Ok(result) => result,
                    Err(e) => return ReconnectPodResult::Unavailable(e),
                };
                self.spawn_reconnect_k8s_siblings(
                    context.clone(),
                    namespace.clone(),
                    (repo_path.to_path_buf(), pod_name.0.clone()),
                );
                ReconnectPodResult::Connected(Box::new(result))
            }
            Host::Localhost | Host::Ssh { .. } => {
                let docker_socket = match docker_host {
                    Host::Ssh { .. } => None,
                    Host::Localhost => Some(default_docker_socket()),
                    Host::Kubernetes { .. } => unreachable!(),
                };
                let executor = match self.host_executor(docker_host) {
                    Ok(executor) => executor,
                    Err(e) => return ReconnectPodResult::Unavailable(e),
                };
                let pod_id = crate::executor::pod_id_for(pod_name, repo_path);
                let status = match executor.status(&pod_id) {
                    Ok(status) => status,
                    Err(e) => return ReconnectPodResult::Unavailable(e),
                };
                if status == PodStatus::Gone {
                    return ReconnectPodResult::Gone(anyhow::anyhow!(
                        "container '{pod_id}' not found"
                    ));
                }
                let forward_ports = devcontainer.forward_ports.clone().unwrap_or_default();
                let ports_attributes = devcontainer.ports_attributes.clone().unwrap_or_default();
                let other_ports_attributes = devcontainer.other_ports_attributes.clone();
                match self.reconnect_docker(
                    pod_name,
                    repo_path,
                    docker_host,
                    &executor,
                    &pod_id,
                    status,
                    docker_socket,
                    &container_repo_path,
                    local_env_vars,
                    &forward_ports,
                    &ports_attributes,
                    &other_ports_attributes,
                    record,
                ) {
                    Ok(result) => ReconnectPodResult::Connected(Box::new(result)),
                    Err(e) => ReconnectPodResult::Unavailable(e),
                }
            }
        }
    }

    /// Reconnect to an existing k8s pod.
    #[allow(clippy::too_many_arguments)]
    fn reconnect_k8s(
        &self,
        pod_name: &PodName,
        repo_path: &Path,
        docker_host: &Host,
        _context: &str,
        _namespace: &str,
        executor: &crate::executor::Executor,
        pod_id: &crate::executor::PodId,
        container_repo_path: &Path,
        record: &db::PodRecord,
    ) -> Result<LaunchResult> {
        let token = record.token.clone();
        let proxy_key = (repo_path.to_path_buf(), pod_name.0.clone());

        // Reuse an existing exec proxy if it's still alive; otherwise
        // start a fresh one.  Mirrors reconnect_docker so a second
        // `rumpel enter` into an existing pod doesn't tear down the
        // connection held by a running `rumpel claude`.
        let mut pod_server_route_changed = false;
        {
            let mut proxies = self.exec_proxies.lock().unwrap();
            if let Some(handle) = proxies.get(&proxy_key) {
                if !handle.is_alive() {
                    proxies.remove(&proxy_key);
                    pod_server_route_changed = true;
                }
            }
        }
        let container_url = {
            let proxies = self.exec_proxies.lock().unwrap();
            proxies
                .get(&proxy_key)
                .map(|h| format!("http://127.0.0.1:{}", h.port))
        };
        let container_url = match container_url {
            Some(url) => url,
            None => {
                let serve_port = read_container_server_port(executor, pod_id)
                    .context("reading server-port file for existing k8s pod")?;
                let proxy = block_on(crate::exec_proxy::start_exec_proxy(
                    (*executor).clone(),
                    (*pod_id).clone(),
                    serve_port,
                ))
                .context("starting k8s exec proxy for existing pod")?;
                let url = format!("http://127.0.0.1:{}", proxy.port);
                self.exec_proxies
                    .lock()
                    .unwrap()
                    .insert(proxy_key.clone(), proxy);
                pod_server_route_changed = true;
                url
            }
        };
        if pod_server_route_changed {
            self.cleanup_codex_runtime(repo_path, &pod_name.0);
        }

        // Readiness check: PodClient::new polls /events.
        let _pod = PodClient::new(&container_url, &token, RetryPolicy::UserBlocking)?;

        // Reuse the existing tunnel if it's still alive, otherwise
        // start a fresh one.
        let tunnel_key = (repo_path.to_path_buf(), pod_name.0.clone());
        {
            let mut tunnels = self.k8s_tunnels.lock().unwrap();
            if let Some(handle) = tunnels.get(&tunnel_key) {
                if !handle.is_alive() {
                    let name = &pod_name.0;
                    log::warn!("k8s tunnel for {name} is dead, reconnecting");
                    tunnels.remove(&tunnel_key);
                }
            }
        }
        {
            let tunnels = self.k8s_tunnels.lock().unwrap();
            if !tunnels.contains_key(&tunnel_key) {
                drop(tunnels);
                let port = self.localhost_server_port;
                let tunnel = block_on(crate::tunnel::start_tunnel(
                    executor,
                    pod_id,
                    &format!("127.0.0.1:{port}"),
                ))
                .context("starting tunnel to existing k8s pod")?;
                self.k8s_tunnels.lock().unwrap().insert(tunnel_key, tunnel);
            }
        }

        {
            let conn = self.db.lock().unwrap();
            db::update_pod_status(&conn, record.id, db::PodStatus::Ready)?;
        }

        // Re-establish forwarded ports from DB only if the daemon
        // doesn't already hold live forwards for this pod.  Without
        // this, a second `rumpel enter` would drop the existing
        // forwards vec and break any TCP connections a user has open
        // against them.
        let forwards_key = (repo_path.to_path_buf(), pod_name.0.clone());
        let already_has_forwards = self
            .port_forward_proxies
            .lock()
            .unwrap()
            .contains_key(&forwards_key);
        if !already_has_forwards {
            let conn = self.db.lock().unwrap();
            let handles = setup_port_forwarding(
                &conn,
                executor,
                pod_id,
                record.id,
                &[],
                &std::collections::HashMap::new(),
                &None,
            )?;
            drop(conn);
            self.port_forward_proxies
                .lock()
                .unwrap()
                .insert(forwards_key, handles);
        }

        self.pod_events.start(
            repo_path.to_path_buf(),
            pod_name.0.clone(),
            container_url.clone(),
            token.clone(),
            docker_host.clone(),
        );

        Ok(LaunchResult {
            container_id: ContainerId(pod_id.as_str().to_string()),
            docker_socket: None,
            host: docker_host.clone(),
            image_built: false,
            container_url,
            container_token: token,
            container_repo_path: container_repo_path.to_path_buf(),
        })
    }

    // Run sibling reconnection off the enter path so the user is not
    // blocked on N kubectl-exec setups.  Siblings race to a steady
    // state on their own; a subsequent enter whose sibling reconnect
    // is still mid-flight finds an alive-or-in-progress tunnel and
    // is a no-op.
    fn spawn_reconnect_k8s_siblings(
        &self,
        context: String,
        namespace: String,
        exclude: (PathBuf, String),
    ) {
        let this = self.clone();
        std::thread::spawn(move || {
            this.reconnect_k8s_siblings(&context, &namespace, &exclude);
        });
    }

    // The daemon does not auto-restore k8s pods on startup, so pending
    // pushes from pods that were running across a restart would only
    // land when the user re-enters each pod individually.  Piggy-back
    // on any enter into cluster+namespace to reconnect the rest, so
    // touching a single pod heals its siblings.
    fn reconnect_k8s_siblings(&self, context: &str, namespace: &str, exclude: &(PathBuf, String)) {
        let pods = {
            let conn = self.db.lock().unwrap();
            match db::list_pods_by_status(&conn, db::PodStatus::Ready) {
                Ok(pods) => pods,
                Err(e) => {
                    eprintln!("reconnect_k8s_siblings: failed to list pods: {e:#}");
                    return;
                }
            }
        };

        for pod in pods {
            let key = (PathBuf::from(&pod.repo_path), pod.name.clone());
            if &key == exclude {
                continue;
            }

            let host: Host = match serde_json::from_str(&pod.host) {
                Ok(h) => h,
                Err(e) => {
                    let name = &pod.name;
                    eprintln!("reconnect_k8s_siblings: parse host for {name}: {e:#}");
                    continue;
                }
            };

            match &host {
                Host::Kubernetes {
                    context: c,
                    namespace: n,
                    ..
                } if c == context && n == namespace => {}
                Host::Kubernetes { .. } | Host::Localhost | Host::Ssh { .. } => continue,
            }

            let already_live = self
                .k8s_tunnels
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|h| h.is_alive());
            if already_live {
                continue;
            }

            let pod_name = PodName(pod.name.clone());
            let repo_path = PathBuf::from(&pod.repo_path);
            let executor = match self.host_executor(&host) {
                Ok(executor) => executor,
                Err(e) => {
                    let name = &pod.name;
                    let path = &pod.repo_path;
                    log::warn!(
                        "reconnect_k8s_siblings: failed to open host for {name} at {path}: {e:#}"
                    );
                    continue;
                }
            };
            let backend_pod_id = crate::executor::pod_id_for(&pod_name, &repo_path);
            let status = match executor.status(&backend_pod_id) {
                Ok(status) => status,
                Err(e) => {
                    let name = &pod.name;
                    let path = &pod.repo_path;
                    log::warn!(
                        "reconnect_k8s_siblings: failed to read status for {name} at {path}: {e:#}"
                    );
                    continue;
                }
            };
            match status {
                PodStatus::Running => {}
                PodStatus::Gone
                | PodStatus::Stopped
                | PodStatus::Disconnected
                | PodStatus::Stopping
                | PodStatus::Deleting
                | PodStatus::Broken => continue,
            }
            // container_repo_path is only threaded into the discarded
            // LaunchResult, so any path works here.
            if let Err(e) = self.reconnect_k8s(
                &pod_name,
                &repo_path,
                &host,
                context,
                namespace,
                &executor,
                &backend_pod_id,
                &repo_path,
                &pod,
            ) {
                let name = &pod.name;
                let path = &pod.repo_path;
                log::warn!("reconnect_k8s_siblings: failed to reconnect {name} at {path}: {e:#}");
            }
        }
    }

    /// Reconnect to an existing Docker container.
    #[allow(clippy::too_many_arguments)]
    fn reconnect_docker(
        &self,
        pod_name: &PodName,
        repo_path: &Path,
        docker_host: &Host,
        executor: &crate::executor::Executor,
        pod_id: &crate::executor::PodId,
        status: PodStatus,
        docker_socket: Option<PathBuf>,
        container_repo_path: &Path,
        local_env_vars: &HashMap<String, String>,
        forward_ports: &[devcontainer::Port],
        ports_attributes: &HashMap<String, devcontainer::PortAttributes>,
        other_ports_attributes: &Option<devcontainer::PortAttributes>,
        record: &db::PodRecord,
    ) -> Result<LaunchResult> {
        let was_stopped = status != PodStatus::Running;
        if was_stopped {
            executor.start(pod_id)?;
        }

        // A proxy from a previous daemon run may still be in the map.
        // If the container was stopped it targets a dead exec session;
        // drop it so we start a fresh one after container-serve is up.
        let proxy_key = (repo_path.to_path_buf(), pod_name.0.clone());
        let mut pod_server_route_changed = was_stopped;
        {
            let mut proxies = self.exec_proxies.lock().unwrap();
            if let Some(handle) = proxies.get(&proxy_key) {
                if was_stopped || !handle.is_alive() {
                    proxies.remove(&proxy_key);
                    pod_server_route_changed = true;
                }
            }
        }

        let token = record.token.clone();
        {
            let conn = self.db.lock().unwrap();
            db::update_pod_status(&conn, record.id, db::PodStatus::Ready)?;
        }

        // Tunnel must come up before container-serve: the latter runs
        // git fetch during its startup and needs the host reachable.
        // If the container was stopped, the tunnel-server inside it is
        // gone even if the mux task hasn't noticed yet, so always start
        // fresh in that case.
        let localhost_server_port = self.localhost_server_port;
        let tunnel_key = (repo_path.to_path_buf(), pod_name.0.clone());
        {
            let mut tunnels = self.docker_tunnels.lock().unwrap();
            if let Some(handle) = tunnels.get(&tunnel_key) {
                if was_stopped || !handle.is_alive() {
                    tunnels.remove(&tunnel_key);
                }
            }
        }
        {
            let tunnels = self.docker_tunnels.lock().unwrap();
            if !tunnels.contains_key(&tunnel_key) {
                drop(tunnels);
                let tunnel = block_on(crate::tunnel::start_tunnel(
                    executor,
                    pod_id,
                    &format!("127.0.0.1:{localhost_server_port}"),
                ))
                .context("starting tunnel to existing docker container")?;
                self.docker_tunnels
                    .lock()
                    .unwrap()
                    .insert(tunnel_key.clone(), tunnel);
            }
        }

        if was_stopped {
            start_container_server(
                executor,
                pod_id,
                container_repo_path,
                &pod_name.0,
                local_env_vars,
                &token,
                None,
            )?;
        }

        let serve_port = read_container_server_port(executor, pod_id)
            .context("reading server-port file for existing container")?;
        let container_url = {
            let existing = {
                let proxies = self.exec_proxies.lock().unwrap();
                proxies.get(&proxy_key).map(|h| (h.port, h.is_alive()))
            };
            match existing {
                Some((port, true)) => format!("http://127.0.0.1:{port}"),
                _ => {
                    let proxy = block_on(crate::exec_proxy::start_exec_proxy(
                        (*executor).clone(),
                        (*pod_id).clone(),
                        serve_port,
                    ))
                    .context("starting exec proxy for existing container")?;
                    let url = format!("http://127.0.0.1:{}", proxy.port);
                    self.exec_proxies
                        .lock()
                        .unwrap()
                        .insert(proxy_key.clone(), proxy);
                    pod_server_route_changed = true;
                    url
                }
            }
        };
        if pod_server_route_changed {
            self.cleanup_codex_runtime(repo_path, &pod_name.0);
        }

        let _pod = PodClient::new(&container_url, &token, RetryPolicy::UserBlocking)?;

        // Set up port forwarding for existing container on re-entry.
        // Skip if handles for this pod are already held; otherwise a
        // second `rumpel enter` would drop live forwards and break
        // TCP connections the user has open.  Even when the devcontainer
        // declares no `forwardPorts`, the DB may carry rows from
        // `rumpel forward-port`, so let `setup_port_forwarding`
        // discover them.
        let forwards_key = (repo_path.to_path_buf(), pod_name.0.clone());
        let already_has_forwards = self
            .port_forward_proxies
            .lock()
            .unwrap()
            .contains_key(&forwards_key);
        if !already_has_forwards {
            let conn = self.db.lock().unwrap();
            let handles = setup_port_forwarding(
                &conn,
                executor,
                pod_id,
                record.id,
                forward_ports,
                ports_attributes,
                other_ports_attributes,
            )?;
            drop(conn);
            self.port_forward_proxies
                .lock()
                .unwrap()
                .insert(forwards_key, handles);
        }

        self.pod_events.start(
            repo_path.to_path_buf(),
            pod_name.0.clone(),
            container_url.clone(),
            token.clone(),
            docker_host.clone(),
        );

        Ok(LaunchResult {
            container_id: ContainerId(pod_id.as_str().to_string()),
            docker_socket,
            host: docker_host.clone(),
            image_built: false,
            container_url,
            container_token: token,
            container_repo_path: container_repo_path.to_path_buf(),
        })
    }

    /// Launch a pod on Kubernetes, given an already-prepared image.
    ///
    /// Image resolution (`resolve_image` + `build_prepared_image`), the
    /// gateway setup, and the `GitSetupParams` are all decided by
    /// `launch_pod_impl` so the prebuilt/fork path can short-circuit
    /// them on both hosts.  This function only owns the k8s-specific
    /// pod creation: create the container, start the exec proxy, start
    /// the git tunnel, start container-serve, wait for readiness,
    /// upload bind mounts, mark ready, set up user-facing port
    /// forwards.  It mirrors the `do_create_and_setup` closure in the
    /// docker path of `launch_pod_impl` and must be kept in sync with
    /// it by hand.
    #[allow(clippy::too_many_arguments)]
    fn launch_pod_k8s(
        &self,
        pod_name: &PodName,
        repo_path: &Path,
        docker_host: &Host,
        devcontainer: &DevContainer,
        image: &str,
        image_built: bool,
        git_setup: &GitSetupParams,
        local_env_vars: &HashMap<String, String>,
        raw_devcontainer_json: &str,
        progress_tx: &std::sync::mpsc::Sender<crate::image::OutputLine>,
    ) -> Result<LaunchResult> {
        let (node_selector, tolerations) = match docker_host {
            Host::Kubernetes {
                node_selector,
                tolerations,
                ..
            } => (node_selector.clone(), tolerations.clone()),
            _ => unreachable!("launch_pod_k8s called with non-Kubernetes host"),
        };

        let container_repo_path = devcontainer.container_repo_path(repo_path);

        let exec_pod_id = crate::executor::pod_id_for(pod_name, repo_path);
        let executor = self.host_executor(docker_host)?;

        let token = SharedGitServerState::generate_token();

        // Create pod record in database and persist token.
        let local_env_json = serialize_local_env(local_env_vars);
        let pod_id = {
            let conn = self.db.lock().unwrap();
            db::create_pod(
                &conn,
                repo_path,
                &pod_name.0,
                docker_host,
                &token,
                image,
                raw_devcontainer_json,
                &local_env_json,
            )?
        };

        let mark_error = |e: anyhow::Error| -> anyhow::Error {
            if let Ok(conn) = self.db.lock() {
                let _ = db::update_pod_status(&conn, pod_id, db::PodStatus::Error);
            }
            e
        };

        let mut bind_sources = Vec::new();
        let spec = build_k8s_pod_spec(
            pod_name,
            image,
            repo_path,
            devcontainer,
            node_selector,
            tolerations,
            &progress_tx.clone(),
            &mut bind_sources,
        )
        .map_err(&mark_error)?;

        progress_tx
            .send(OutputLine::Stderr("Creating container...".into()))
            .ok();
        executor
            .launch(&exec_pod_id, spec)
            .map_err(|e| mark_error(e.context("creating k8s pod")))?;

        // Start exec tunnel so the container can reach the git HTTP
        // server on a loopback port.  Must be up before container-serve
        // starts, because container-serve clones the repo at startup.
        let tunnel = block_on(crate::tunnel::start_tunnel(
            &executor,
            &exec_pod_id,
            &format!("127.0.0.1:{}", self.localhost_server_port),
        ))
        .map_err(|e| mark_error(e.context("starting tunnel to k8s pod")))?;
        self.k8s_tunnels
            .lock()
            .unwrap()
            .insert((repo_path.to_path_buf(), pod_name.0.clone()), tunnel);

        // Start container-serve with git-init params.  It clones the
        // repo, sets up git remotes/hooks, and runs lifecycle commands
        // during startup, so it only accepts connections once ready.
        progress_tx
            .send(OutputLine::Stderr("Starting container server...".into()))
            .ok();
        start_container_server(
            &executor,
            &exec_pod_id,
            &container_repo_path,
            &pod_name.0,
            local_env_vars,
            &token,
            Some(git_setup),
        )
        .map_err(mark_error)?;

        // Container-serve picks its port at startup and writes it to
        // /opt/rumpelpod/server-port; read it before building the
        // exec proxy so the per-connection `rumpel tcp-proxy` targets
        // the right loopback port.
        let serve_port = read_container_server_port(&executor, &exec_pod_id)
            .map_err(|e| mark_error(e.context("reading server-port file from new k8s pod")))?;
        let proxy = block_on(crate::exec_proxy::start_exec_proxy(
            executor.clone(),
            exec_pod_id.clone(),
            serve_port,
        ))
        .map_err(|e| mark_error(e.context("starting exec proxy for container-serve")))?;
        let container_url = format!("http://127.0.0.1:{}", proxy.port);
        self.exec_proxies
            .lock()
            .unwrap()
            .insert((repo_path.to_path_buf(), pod_name.0.clone()), proxy);

        let progress_for_wait = progress_tx.clone();
        let pod = PodClient::wait_and_connect(&container_url, &token, |msg| {
            let _ = progress_for_wait.send(OutputLine::Stderr(msg.to_string()));
        })
        .map_err(mark_error)?;

        // Populate bind mount volumes with data from the local machine.
        upload_bind_mounts(&pod, &bind_sources)
            .map_err(|e| mark_error(e.context("populating bind mount volumes")))?;

        {
            let conn = self.db.lock().unwrap();
            db::update_pod_status(&conn, pod_id, db::PodStatus::Ready)?;
        }

        // Set up exec-proxy listeners for devcontainer forwardPorts.
        let forward_ports = devcontainer.forward_ports.as_deref().unwrap_or(&[]);
        let ports_attributes = devcontainer
            .ports_attributes
            .as_ref()
            .cloned()
            .unwrap_or_default();
        let other_ports_attributes = &devcontainer.other_ports_attributes;

        let handles = {
            let conn = self.db.lock().unwrap();
            setup_port_forwarding(
                &conn,
                &executor,
                &exec_pod_id,
                pod_id,
                forward_ports,
                &ports_attributes,
                other_ports_attributes,
            )?
        };
        self.port_forward_proxies
            .lock()
            .unwrap()
            .insert((repo_path.to_path_buf(), pod_name.0.clone()), handles);

        self.pod_events.start(
            repo_path.to_path_buf(),
            pod_name.0.clone(),
            container_url.clone(),
            token.clone(),
            docker_host.clone(),
        );

        Ok(LaunchResult {
            container_id: ContainerId(exec_pod_id.as_str().to_string()),
            docker_socket: None,
            host: docker_host.clone(),
            image_built,
            container_url,
            container_token: token,
            container_repo_path: container_repo_path.to_path_buf(),
        })
    }

    /// Fresh-pod launch path used by `rumpel enter` and `rumpel recreate`.
    ///
    /// Resolves the devcontainer + image + git_setup from the host repo,
    /// then dispatches to `create_pod_container` for the docker/k8s
    /// creation step.  Build output lines are sent to `build_tx`; the
    /// caller drives the `ServerLaunchProgress` iterator to forward them
    /// to the client.
    fn launch_pod_impl(
        &self,
        params: PodLaunchParams,
        build_tx: std::sync::mpsc::Sender<crate::image::OutputLine>,
    ) -> Result<LaunchResult> {
        let PodLaunchParams {
            pod_name,
            repo_path,
            host_branch,
            host: docker_host,
            git_identity,
            claude_cli_path,
            codex_cli_path,
            pi_cli_path,
            grok_cli_path,
            inject_system_prompt,
            description_file,
            local_env_vars,
            ssh_auth_sock,
        } = params;

        let (mut devcontainer, used_default_image) =
            load_and_resolve_devcontainer(&repo_path, &pod_name.0, &local_env_vars)?;
        let raw_devcontainer_json =
            DevContainer::find_raw(&repo_path)?.unwrap_or_else(|| "{}".to_string());

        if docker_host.is_remote() {
            validate_bind_mount_ownership(&devcontainer)?;
        }

        if let Some(ref requirements) = devcontainer.host_requirements {
            if let Some(msg) = host_requirements_message(requirements, &docker_host) {
                build_tx.send(OutputLine::Stderr(msg)).ok();
            }
        }

        // When re-entering an existing pod, use the host it was created on
        // rather than whatever the current config resolves to.  The pod is
        // already running somewhere; reconnect to it there.
        //
        // If the pod exists in the DB, skip image builds entirely and
        // reconnect.  If the pod turns out to be gone (k8s eviction,
        // Docker removal), clean up the stale DB record and fall through
        // to a fresh create.
        let mut docker_host = docker_host;
        // Wait for any in-progress background stop to finish before
        // checking the DB for reentry or creating a new pod.
        for _ in 0..50 {
            let conn = self.db.lock().unwrap();
            let is_stopping = db::get_pod(&conn, &repo_path, &pod_name.0)?
                .is_some_and(|r| r.status == db::PodStatus::Stopping);
            drop(conn);
            if !is_stopping {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        {
            let conn = self.db.lock().unwrap();
            if let Some(existing) = db::get_pod(&conn, &repo_path, &pod_name.0)? {
                let host_spec = serde_json::to_string(&docker_host)?;
                if existing.host != host_spec {
                    let existing_host: Host = serde_json::from_str(&existing.host)
                        .context("parsing stored host for existing pod")?;
                    eprintln!(
                        "Pod '{}' exists on {}, reconnecting there.",
                        pod_name.0, existing_host,
                    );
                    docker_host = existing_host;
                }
                drop(conn);
                match self.reconnect_pod(
                    &pod_name,
                    &repo_path,
                    &docker_host,
                    &devcontainer,
                    &local_env_vars,
                    &existing,
                ) {
                    ReconnectPodResult::Connected(result) => return Ok(*result),
                    ReconnectPodResult::Gone(e) => {
                        log::warn!("existing pod is gone, will recreate: {e:#}");
                        self.cleanup_codex_runtime(&repo_path, &pod_name.0);
                        let conn = self.db.lock().unwrap();
                        db::delete_pod(&conn, &repo_path, &pod_name.0)?;
                    }
                    ReconnectPodResult::Unavailable(e) => {
                        return Err(e.context(format!(
                            "reconnecting existing pod '{}' failed",
                            pod_name.0
                        )));
                    }
                }
            }
        }

        for warning in devcontainer.unsupported_field_warnings() {
            build_tx.send(OutputLine::Stderr(warning)).ok();
        }

        // Stage the embedded Dockerfile only once we are certain we
        // will build.  The tempdir is held for the rest of the
        // function so `resolve_image` can walk and read from it; it
        // drops together with `launch_pod_impl` when we return.
        let _default_image_dir = if used_default_image {
            build_tx
                .send(OutputLine::Stderr(
                    "warning: no image or build configured, building default image".into(),
                ))
                .ok();
            Some(stage_default_image(&mut devcontainer)?)
        } else {
            None
        };

        let all_mounts = devcontainer.resolved_mounts()?;
        check_no_unresolved_mount_vars(&all_mounts)?;
        let mount_targets: Vec<String> = all_mounts.iter().map(|m| m.target.clone()).collect();
        let (mounts, bind_sources) = split_bind_mounts(all_mounts, &docker_host, &pod_name.0);

        let container_repo_path = devcontainer.container_repo_path(&repo_path);
        let git_dir = fs::canonicalize(repo_path.join(".git")).context("resolving .git dir")?;

        let docker_socket: Option<PathBuf> = match &docker_host {
            Host::Ssh { .. } => None,
            Host::Localhost => Some(default_docker_socket()),
            Host::Kubernetes { .. } => None,
        };
        let executor = self.host_executor(&docker_host)?;

        gateway::setup_gateway(&repo_path)?;

        build_tx
            .send(OutputLine::Stderr("Resolving image...".into()))
            .ok();
        // The default image is staged into a moving tempdir, so its tag
        // must ignore the context path; every real context is at the
        // stable repo root and folds its path into the tag so distinct
        // repos do not collide on one cached image.
        let path_tagging = if used_default_image {
            crate::image::ContextPathTagging::Ignore
        } else {
            crate::image::ContextPathTagging::Include
        };
        let build_result = crate::image::resolve_image(
            &devcontainer,
            &docker_host,
            &repo_path,
            &crate::image::BuildFlags::default(),
            path_tagging,
            make_build_output(&build_tx),
            docker_socket.as_deref(),
            ssh_auth_sock.as_deref(),
        )?;
        let host_remotes = crate::git::get_remotes(&repo_path).unwrap_or_default();
        let build_options = devcontainer.build_options();
        build_tx
            .send(OutputLine::Stderr("Preparing image...".into()))
            .ok();
        let container_env_keys = container_env_keys_sorted(&devcontainer);
        let prepared = crate::prepared_image::build_prepared_image(
            &build_result.image,
            &docker_host,
            &git_dir,
            &container_repo_path,
            devcontainer.user(),
            &host_remotes,
            &mount_targets,
            claude_cli_path.as_deref(),
            codex_cli_path.as_deref(),
            pi_cli_path.as_deref(),
            grok_cli_path.as_deref(),
            docker_socket.as_deref(),
            inject_system_prompt,
            description_file.as_deref(),
            &raw_devcontainer_json,
            &container_env_keys,
            &build_options,
            ssh_auth_sock.as_deref(),
            make_build_output(&build_tx),
        )?;
        let image = prepared.image;
        let image_built = build_result.built || prepared.built;

        gateway::install_host_hooks(&repo_path)?;

        let git_setup = fresh_pod_git_setup(&pod_name.0, host_branch.as_deref(), git_identity);

        self.create_pod_container(
            ResolvedLaunch {
                pod_name,
                repo_path,
                docker_host,
                devcontainer,
                raw_devcontainer_json,
                image,
                image_built,
                git_setup,
                local_env_vars,
                mounts,
                bind_sources,
                container_repo_path,
                executor,
                docker_socket,
            },
            build_tx,
        )
    }

    /// Fork-mode launch path: the caller (only `fork_pod_impl`) supplies
    /// the source pod's image, raw devcontainer.json, container-env
    /// snapshot, and a fork-aware `GitSetupParams` directly.  This
    /// skips image resolution, host-disk reads, and the reentry check
    /// (the new pod name has already been validated as unused).
    #[allow(clippy::too_many_arguments)]
    fn launch_pod_from_source(
        &self,
        pod_name: PodName,
        repo_path: PathBuf,
        docker_host: Host,
        local_env_vars: HashMap<String, String>,
        source_image: String,
        source_devcontainer_json: String,
        source_env: HashMap<String, String>,
        git_setup: GitSetupParams,
        build_tx: std::sync::mpsc::Sender<crate::image::OutputLine>,
    ) -> Result<LaunchResult> {
        // Parse the source pod's stored devcontainer.json instead of
        // reading it from the host repo (which may have changed).  The
        // source pod's `/container-env` snapshot is overlaid onto
        // `dc.container_env` here so the executor passes the source's
        // actual values (including `--env-file` contents as the source
        // saw them) into the new container.  Without this, env-file-
        // derived vars would silently disappear because the stored raw
        // devcontainer.json keeps `--env-file` in `runArgs` and we no
        // longer re-read it from disk.
        let mut devcontainer = parse_prebuilt_devcontainer(
            &source_devcontainer_json,
            &repo_path,
            &pod_name.0,
            &local_env_vars,
        )?;
        if !source_env.is_empty() {
            let env_map = devcontainer.container_env.get_or_insert_with(HashMap::new);
            for (k, v) in source_env {
                env_map.insert(k, v);
            }
        }

        if docker_host.is_remote() {
            validate_bind_mount_ownership(&devcontainer)?;
        }

        if let Some(ref requirements) = devcontainer.host_requirements {
            if let Some(msg) = host_requirements_message(requirements, &docker_host) {
                build_tx.send(OutputLine::Stderr(msg)).ok();
            }
        }

        for warning in devcontainer.unsupported_field_warnings() {
            build_tx.send(OutputLine::Stderr(warning)).ok();
        }

        let all_mounts = devcontainer.resolved_mounts()?;
        check_no_unresolved_mount_vars(&all_mounts)?;
        let (mounts, bind_sources) = split_bind_mounts(all_mounts, &docker_host, &pod_name.0);
        let container_repo_path = devcontainer.container_repo_path(&repo_path);

        let docker_socket: Option<PathBuf> = match &docker_host {
            Host::Ssh { .. } => None,
            Host::Localhost => Some(default_docker_socket()),
            Host::Kubernetes { .. } => None,
        };
        let executor = self.host_executor(&docker_host)?;

        gateway::setup_gateway(&repo_path)?;
        preflight_image_present(&executor, &source_image)?;
        gateway::install_host_hooks(&repo_path)?;

        self.create_pod_container(
            ResolvedLaunch {
                pod_name,
                repo_path,
                docker_host,
                devcontainer,
                raw_devcontainer_json: source_devcontainer_json,
                image: Image(source_image),
                image_built: false,
                git_setup,
                local_env_vars,
                mounts,
                bind_sources,
                container_repo_path,
                executor,
                docker_socket,
            },
            build_tx,
        )
    }

    /// Create the container from already-resolved inputs.
    ///
    /// Both the fresh-pod and fork-mode paths converge here once they
    /// have an image, devcontainer, and git_setup in hand.  The docker
    /// startup flow lives inside `do_create_and_setup` below and is
    /// mirrored by `launch_pod_k8s` for the k8s path.  The two are
    /// duplicated today and must be kept in sync by hand: any change
    /// to the shape of the startup sequence (exec-proxy vs tunnel
    /// ordering, when container-serve starts, readiness waiting,
    /// bind-mount upload) should be reflected in both.
    fn create_pod_container(
        &self,
        resolved: ResolvedLaunch,
        progress_tx: std::sync::mpsc::Sender<crate::image::OutputLine>,
    ) -> Result<LaunchResult> {
        let ResolvedLaunch {
            pod_name,
            repo_path,
            docker_host,
            devcontainer,
            raw_devcontainer_json,
            image,
            image_built,
            git_setup,
            local_env_vars,
            mounts,
            bind_sources,
            container_repo_path,
            executor,
            docker_socket,
        } = resolved;

        let forward_ports = devcontainer.forward_ports.clone().unwrap_or_default();
        let ports_attributes = devcontainer.ports_attributes.clone().unwrap_or_default();
        let other_ports_attributes = devcontainer.other_ports_attributes.clone();

        if let Host::Kubernetes {
            context, namespace, ..
        } = &docker_host
        {
            let result = self.launch_pod_k8s(
                &pod_name,
                &repo_path,
                &docker_host,
                &devcontainer,
                &image.0,
                image_built,
                &git_setup,
                &local_env_vars,
                &raw_devcontainer_json,
                &progress_tx,
            )?;
            self.spawn_reconnect_k8s_siblings(
                context.clone(),
                namespace.clone(),
                (repo_path.clone(), pod_name.0.clone()),
            );
            return Ok(result);
        }

        let exec_pod_id = crate::executor::pod_id_for(&pod_name, &repo_path);
        let localhost_server_port = self.localhost_server_port;

        let token = SharedGitServerState::generate_token();

        let local_env_json = serialize_local_env(&local_env_vars);
        let pod_id = {
            let conn = self.db.lock().unwrap();
            db::create_pod(
                &conn,
                &repo_path,
                &pod_name.0,
                &docker_host,
                &token,
                &image.0,
                &raw_devcontainer_json,
                &local_env_json,
            )?
        };

        let mark_error = |e: anyhow::Error| -> anyhow::Error {
            if let Ok(conn) = self.db.lock() {
                let _ = db::update_pod_status(&conn, pod_id, db::PodStatus::Error);
            }
            e
        };

        // forwardPorts are served via exec-proxy after the container
        // is up, not via `-p` publish at create time; pass an empty
        // map so the spec contains no port bindings.
        let publish_ports: HashMap<u16, u16> = HashMap::new();

        let docker_tunnels = &self.docker_tunnels;
        let exec_proxies = &self.exec_proxies;

        // Create container and run initial git setup.  Closure used so we
        // can retry once on overlay2 filesystem errors (see below).
        let do_create_and_setup = || -> Result<(ContainerId, String)> {
            progress_tx
                .send(OutputLine::Stderr("Creating container...".into()))
                .ok();
            let spec = build_docker_pod_spec(
                &pod_name,
                &image,
                &repo_path,
                &container_repo_path,
                &devcontainer,
                &mounts,
                &publish_ports,
            )?;
            executor.launch(&exec_pod_id, spec)?;
            let container_id = ContainerId(exec_pod_id.as_str().to_string());

            // Start exec tunnel so the container can reach the git HTTP
            // server on a loopback port.  Must be up before container-serve
            // starts, because container-serve clones the repo at startup.
            let tunnel = block_on(crate::tunnel::start_tunnel(
                &executor,
                &exec_pod_id,
                &format!("127.0.0.1:{localhost_server_port}"),
            ))
            .context("starting tunnel to docker container")?;
            docker_tunnels
                .lock()
                .unwrap()
                .insert((repo_path.to_path_buf(), pod_name.0.clone()), tunnel);

            // Start container-serve with git-init params.  It clones the
            // repo, sets up git remotes/hooks, and runs lifecycle commands
            // during startup, so it only accepts connections once ready.
            progress_tx
                .send(OutputLine::Stderr("Starting container server...".into()))
                .ok();
            start_container_server(
                &executor,
                &exec_pod_id,
                &container_repo_path,
                &pod_name.0,
                &local_env_vars,
                &token,
                Some(&git_setup),
            )?;

            // Route container-serve access through an exec proxy so we
            // don't need bridge IPs or SSH port forwards.
            let serve_port = read_container_server_port(&executor, &exec_pod_id)
                .context("reading server-port file for new container")?;
            let proxy = block_on(crate::exec_proxy::start_exec_proxy(
                executor.clone(),
                exec_pod_id.clone(),
                serve_port,
            ))
            .context("starting exec proxy for container-serve")?;
            let port = proxy.port;
            let container_url_inner = format!("http://127.0.0.1:{port}");
            exec_proxies
                .lock()
                .unwrap()
                .insert((repo_path.to_path_buf(), pod_name.0.clone()), proxy);

            let progress_for_wait = progress_tx.clone();
            let pod_inner = PodClient::wait_and_connect(&container_url_inner, &token, |msg| {
                let _ = progress_for_wait.send(OutputLine::Stderr(msg.to_string()));
            })?;

            // Populate bind mount volumes with data from the local machine.
            upload_bind_mounts(&pod_inner, &bind_sources)
                .context("populating bind mount volumes")?;

            Ok((container_id, container_url_inner))
        };

        // Docker's overlay2 storage driver occasionally fails to make the
        // container filesystem visible right after creation.  Retry once
        // after removing the broken container.
        let (container_id, container_url) = match do_create_and_setup() {
            Ok(pair) => pair,
            Err(first_err) if is_overlay2_setup_error(&first_err) => {
                error!(
                    "overlay2 setup error, removing container and retrying: {:#}",
                    first_err
                );
                if let Err(e) = executor.delete(&exec_pod_id) {
                    error!("failed to remove broken container {exec_pod_id}: {e}");
                }
                do_create_and_setup().map_err(|e| {
                    mark_error(e.context(
                        "container setup failed again after retry; this is a known \
                         Docker/overlay2 limitation -- please retry",
                    ))
                })?
            }
            Err(e) => return Err(mark_error(e)),
        };

        let _pod = PodClient::new(&container_url, &token, RetryPolicy::UserBlocking)?;

        // Mark pod as ready and set up exec-proxy listeners for
        // devcontainer forwardPorts.
        let port_forward_handles = {
            let conn = self.db.lock().unwrap();
            db::update_pod_status(&conn, pod_id, db::PodStatus::Ready)?;

            if forward_ports.is_empty() {
                Vec::new()
            } else {
                progress_tx
                    .send(OutputLine::Stderr("Setting up port forwarding...".into()))
                    .ok();
                setup_port_forwarding(
                    &conn,
                    &executor,
                    &exec_pod_id,
                    pod_id,
                    &forward_ports,
                    &ports_attributes,
                    &other_ports_attributes,
                )
                .map_err(|e| {
                    error!("port forwarding setup failed: {e}");
                    e
                })?
            }
        };
        self.port_forward_proxies.lock().unwrap().insert(
            (repo_path.to_path_buf(), pod_name.0.clone()),
            port_forward_handles,
        );

        self.pod_events.start(
            repo_path.to_path_buf(),
            pod_name.0.clone(),
            container_url.clone(),
            token.clone(),
            docker_host.clone(),
        );

        Ok(LaunchResult {
            container_id,
            docker_socket,
            host: docker_host,
            image_built,
            container_url,
            container_token: token,
            container_repo_path,
        })
    }

    /// Core fork logic.  Runs on a worker thread off the axum request
    /// task; the CLI blocks on the launch-progress SSE stream until
    /// this returns, so the new pod is fully ready by then.
    ///
    /// 1. Validate names; load source row from DB.
    /// 2. Read agent state (no HTTP); enforce `--allow-processing` for
    ///    mid-turn sources.
    /// 3. Push source's branches to the host so the new pod can fetch
    ///    them; then GET /state and /git/patch.
    /// 4. Build a fork-aware `GitSetupParams` (rewritten upstreams) and
    ///    hand it plus the source pod's image, raw devcontainer.json,
    ///    and container-env snapshot to `launch_pod_from_source`.
    /// 5. Stream agent files from source to new pod, apply the dirty
    ///    patch, and inherit `claude_config_copied`.
    fn fork_pod_impl(
        &self,
        request: ForkPodRequest,
        build_tx: std::sync::mpsc::Sender<crate::image::OutputLine>,
    ) -> Result<LaunchResult> {
        let ForkPodRequest {
            source,
            new_name,
            repo_path,
            allow_processing,
        } = request;

        crate::cli::validate_pod_name(&new_name)
            .map_err(|e| anyhow::anyhow!("invalid new pod name '{new_name}': {e}"))?;

        let (source_record, new_exists) = {
            let conn = self.db.lock().unwrap();
            let src = db::get_pod(&conn, &repo_path, &source)?
                .with_context(|| format!("source pod '{source}' does not exist"))?;
            let dst = db::get_pod(&conn, &repo_path, &new_name)?.is_some();
            (src, dst)
        };
        if source == new_name {
            return Err(anyhow::anyhow!("source and new name must differ"));
        }
        if new_exists {
            return Err(anyhow::anyhow!("pod '{new_name}' already exists"));
        }
        if source_record.status != db::PodStatus::Ready {
            return Err(anyhow::anyhow!(
                "source pod '{source}' is not running (status: {:?}); start it first",
                source_record.status
            ));
        }

        // v1 lock: fork on the source's host, ignore any explicit --host.
        let docker_host: Host = serde_json::from_str(&source_record.host)
            .context("parsing source pod's stored host")?;

        // Bail mid-turn unless the user opted in.  TTY confirmation
        // happens client-side; the daemon only enforces the policy.
        let claude_state = self.pod_events.claude_state(&repo_path, &source);
        let codex_state = self.pod_events.codex_state(&repo_path, &source);
        let claude_processing = matches!(claude_state, Some(ClaudeState::Processing));
        let codex_processing = matches!(codex_state, Some(CodexState::Processing));
        if (claude_processing || codex_processing) && !allow_processing {
            let agents = match (claude_processing, codex_processing) {
                (true, true) => "claude and codex",
                (true, false) => "claude",
                (false, true) => "codex",
                (false, false) => unreachable!(),
            };
            return Err(anyhow::anyhow!(
                "source pod '{source}' is processing ({agents}); \
                 pass --allow-processing to fork anyway"
            ));
        }

        // Connect to the source pod via its existing exec proxy.
        let source_url = self
            .exec_proxies
            .lock()
            .unwrap()
            .get(&(repo_path.to_path_buf(), source.clone()))
            .map(|h| format!("http://127.0.0.1:{}", h.port))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "source pod '{source}' has no live exec proxy; restart the daemon \
                     or re-enter the pod and try again"
                )
            })?;
        let source_pod =
            PodClient::new(&source_url, &source_record.token, RetryPolicy::UserBlocking)?;

        // Flush any local-only branches to the host before fetching.
        let _ = build_tx.send(OutputLine::Stderr(
            "Pushing source pod's branches to the host...".into(),
        ));
        source_pod.git_push().context("git push on source pod")?;

        let _ = build_tx.send(OutputLine::Stderr("Reading source pod state...".into()));
        let state = source_pod.get_state().context("GET /state on source pod")?;
        let source_env = source_pod
            .get_container_env()
            .context("GET /container-env on source pod")?;

        let dirty_patch = if state.dirty {
            let _ = build_tx.send(OutputLine::Stderr(
                "Capturing source pod's dirty working tree...".into(),
            ));
            let p = source_pod
                .git_patch_get()
                .context("GET /git/patch on source pod")?;
            if p.is_empty() {
                None
            } else {
                Some(p)
            }
        } else {
            None
        };

        // Snapshot agent state up-front; PUT happens after the new pod
        // is Ready.  Buffered as gzip in memory (matches recreate's
        // approach for the same reason: the source could go away while
        // we are bringing the new one up).
        let mut agent_buffers: Vec<(&'static str, Vec<u8>)> = Vec::new();
        if state.has_claude_state {
            if let Some(buf) = snapshot_agent_files(&source_pod, "claude")
                .context("downloading claude state from source")?
            {
                agent_buffers.push(("claude", buf));
            }
        }
        if state.has_codex_state {
            if let Some(buf) = snapshot_agent_files(&source_pod, "codex")
                .context("downloading codex state from source")?
            {
                agent_buffers.push(("codex", buf));
            }
        }
        if state.has_grok_state {
            if let Some(buf) = snapshot_agent_files(&source_pod, "grok")
                .context("downloading grok state from source")?
            {
                agent_buffers.push(("grok", buf));
            }
        }

        // Build the new pod's branch list from the source's branches,
        // applying the upstream-rewriting rules in `rewrite_upstream`.
        let branch_set: std::collections::HashSet<String> =
            state.branches.iter().map(|b| b.name.clone()).collect();
        let mut branches: Vec<crate::pod::git_setup::GitSetupBranch> = Vec::new();
        for src_branch in &state.branches {
            // Primary branch is renamed S -> N; secondaries keep their names.
            let new_name_for_branch = if src_branch.name == state.primary {
                new_name.clone()
            } else {
                src_branch.name.clone()
            };
            let base = format!("source-pod/{}", src_branch.name);
            let upstream = rewrite_upstream(
                src_branch.upstream.as_deref(),
                &state.primary,
                &new_name,
                &branch_set,
            );
            branches.push(crate::pod::git_setup::GitSetupBranch {
                name: new_name_for_branch,
                base,
                upstream,
            });
        }

        let extra_host_fetch = vec![format!(
            "+refs/rumpelpod/*@{source}:refs/remotes/source-pod/*"
        )];

        let local_env_vars = deserialize_local_env(&source_record.local_env)?;

        let git_identity = crate::git::get_git_user_config(&repo_path);
        let git_setup = crate::pod::types::GitSetupParams {
            branches,
            primary: new_name.clone(),
            extra_host_fetch,
            git_identity: Some(git_identity),
        };

        let result = self.launch_pod_from_source(
            PodName(new_name.clone()),
            repo_path.clone(),
            docker_host,
            local_env_vars,
            source_record.image.clone(),
            source_record.devcontainer_json.clone(),
            source_env,
            git_setup,
            build_tx,
        )?;

        // New pod is up -- restore agent state and dirty patch.
        let new_pod = PodClient::new(
            &result.container_url,
            &result.container_token,
            RetryPolicy::UserBlocking,
        )?;

        for (agent, buf) in agent_buffers {
            new_pod
                .put_agent_files(agent, std::io::Cursor::new(buf), None)
                .with_context(|| format!("uploading {agent} state to new pod"))?;
        }

        if let Some(p) = dirty_patch {
            new_pod
                .git_patch_apply(&p)
                .context("applying source's dirty patch to new pod")?;
        }

        // Inherit claude_config_copied so the next `rumpel claude <new>`
        // does not clobber the freshly-restored claude state by re-running
        // copy_claude_config_via_pod.
        let conn = self.db.lock().unwrap();
        if db::has_claude_config_copied(&conn, source_record.id)? {
            let new_record = db::get_pod(&conn, &repo_path, &new_name)?
                .context("new pod row missing right after creation")?;
            db::mark_claude_config_copied(&conn, new_record.id)?;
        }

        Ok(result)
    }

    /// Core recreate logic, called on a background thread.
    ///
    /// Calls `launch_pod_impl` directly to avoid spawning a nested thread.
    fn recreate_pod_impl(
        &self,
        params: PodLaunchParams,
        build_tx: std::sync::mpsc::Sender<crate::image::OutputLine>,
    ) -> Result<LaunchResult> {
        let pod_name = &params.pod_name;
        let repo_path = &params.repo_path;
        let docker_host = &params.host;

        if let Host::Kubernetes { .. } = docker_host {
            let pod_id = crate::executor::pod_id_for(pod_name, repo_path);
            let executor = self.host_executor(docker_host)?;

            // 1. Snapshot dirty files and per-agent state if the pod is running
            let mut patch: Option<Vec<u8>> = None;
            let mut agent_snapshots: Vec<(&'static str, Vec<u8>)> = Vec::new();

            let status = executor.status(&pod_id)?;
            if status == PodStatus::Running {
                // Snapshot through the already-running exec proxy if
                // the daemon has one; otherwise skip snapshotting.
                // The proxy is normally set up by launch_pod_k8s /
                // reconnect_k8s before recreate is invoked.
                let local_port = self
                    .exec_proxies
                    .lock()
                    .unwrap()
                    .get(&(repo_path.to_path_buf(), pod_name.0.clone()))
                    .map(|h| h.port);

                if let Some(port) = local_port {
                    let container_url = format!("http://127.0.0.1:{port}");
                    let token_out = executor.exec(
                        &pod_id,
                        crate::executor::ExecRequest {
                            cmd: vec!["cat".into(), crate::pod::TOKEN_FILE.to_string()],
                            workdir: None,
                            env: Vec::new(),
                            stdin: None,
                        },
                    );
                    if let Ok(out) = token_out {
                        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if let Ok(old_pod) =
                            PodClient::new(&container_url, &token, RetryPolicy::Background)
                        {
                            let p = old_pod
                                .git_patch_get()
                                .context("snapshotting dirty files in k8s pod")?;
                            patch = if p.is_empty() { None } else { Some(p) };

                            for agent in AGENT_NAMES {
                                if let Some(buf) = snapshot_agent_files(&old_pod, agent)
                                    .with_context(|| format!("snapshotting {agent} state"))?
                                {
                                    agent_snapshots.push((agent, buf));
                                }
                            }
                        }
                    }
                }
            }

            // 2. Delete the pod
            Daemon::delete_pod(self, pod_name.clone(), repo_path.clone(), true)?;

            // 3. Create new pod (call impl directly to avoid nested thread)
            let launch_result = self.launch_pod_impl(params, build_tx)?;

            // 4. Restore snapshots
            if patch.is_some() || !agent_snapshots.is_empty() {
                let new_pod = PodClient::new(
                    &launch_result.container_url,
                    &launch_result.container_token,
                    RetryPolicy::UserBlocking,
                )?;

                if let Some(patch_content) = patch {
                    new_pod
                        .git_patch_apply(&patch_content)
                        .context("applying snapshot patch to new k8s pod")?;
                }

                for (agent, buf) in agent_snapshots {
                    new_pod
                        .put_agent_files(agent, std::io::Cursor::new(buf), None)
                        .with_context(|| format!("restoring {agent} state to new k8s pod"))?;
                }
            }

            return Ok(launch_result);
        }

        let executor = self.host_executor(docker_host)?;
        let pod_id = crate::executor::pod_id_for(pod_name, repo_path);

        // 1. Snapshot dirty files and per-agent state if container exists
        let mut patch: Option<Vec<u8>> = None;
        let mut agent_snapshots: Vec<(&'static str, Vec<u8>)> = Vec::new();

        let status = executor.status(&pod_id)?;
        let exists = status != PodStatus::Gone;
        if exists {
            if status == PodStatus::Running {
                // Look up the old pod's token so we can authenticate to
                // its server for snapshotting.
                let old_token = {
                    let conn = self.db.lock().unwrap();
                    db::get_pod(&conn, repo_path, &pod_name.0)
                        .ok()
                        .flatten()
                        .map(|r| r.token)
                };
                if let Some(old_token) = old_token {
                    if let Ok(serve_port) = read_container_server_port(&executor, &pod_id) {
                        if let Ok(proxy) = block_on(crate::exec_proxy::start_exec_proxy(
                            executor.clone(),
                            pod_id.clone(),
                            serve_port,
                        )) {
                            let port = proxy.port;
                            let url = format!("http://127.0.0.1:{port}");
                            if let Ok(old_pod) =
                                PodClient::new(&url, &old_token, RetryPolicy::Background)
                            {
                                let p = old_pod
                                    .git_patch_get()
                                    .context("snapshotting dirty files")?;
                                patch = if p.is_empty() { None } else { Some(p) };

                                for agent in AGENT_NAMES {
                                    if let Some(buf) = snapshot_agent_files(&old_pod, agent)
                                        .with_context(|| format!("snapshotting {agent} state"))?
                                    {
                                        agent_snapshots.push((agent, buf));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // 2. Delete the container synchronously so launch_pod can reuse the name
            Daemon::delete_pod(self, pod_name.clone(), repo_path.clone(), true)?;
        }

        // 3. Create new pod (call impl directly to avoid nested thread)
        let launch_result = self.launch_pod_impl(params, build_tx)?;

        // 4. Restore snapshots
        if patch.is_some() || !agent_snapshots.is_empty() {
            let new_pod = PodClient::new(
                &launch_result.container_url,
                &launch_result.container_token,
                RetryPolicy::UserBlocking,
            )?;

            if let Some(patch_content) = patch {
                new_pod
                    .git_patch_apply(&patch_content)
                    .context("applying snapshot patch")?;
            }

            for (agent, buf) in agent_snapshots {
                new_pod
                    .put_agent_files(agent, std::io::Cursor::new(buf), None)
                    .with_context(|| format!("restoring {agent} state"))?;
            }
        }

        Ok(launch_result)
    }

    /// Re-establish connections to pods that were running before the
    /// daemon restarted.  Without this, commits made while the daemon
    /// was down would never be pushed to the host repo until someone
    /// manually re-entered the pod.
    fn restore_running_pods(&self) {
        let pods = {
            let conn = self.db.lock().unwrap();
            match db::list_pods_by_status(&conn, db::PodStatus::Ready) {
                Ok(pods) => pods,
                Err(e) => {
                    eprintln!("restore_running_pods: failed to list pods: {e:#}");
                    return;
                }
            }
        };

        for pod in pods {
            let host: Host = match serde_json::from_str(&pod.host) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!(
                        "restore_running_pods: failed to parse host for {}: {e:#}",
                        pod.name
                    );
                    continue;
                }
            };

            match host {
                Host::Localhost => {}
                // SSH pods reconnect lazily when the host answers
                // dial-stdio again, driven by the pod_event_loop.
                Host::Ssh { .. } => continue,
                // TODO: decide on a better reconnection policy for k8s
                // pods.  For now, skip them like SSH pods.
                Host::Kubernetes { .. } => continue,
            }

            if let Err(e) = self.restore_one_pod(&pod) {
                eprintln!(
                    "restore_running_pods: failed to restore {}: {e:#}",
                    pod.name
                );
            }
        }
    }

    fn restore_one_pod(&self, pod: &db::PodRecord) -> Result<()> {
        let repo_path = PathBuf::from(&pod.repo_path);
        let pod_name = PodName(pod.name.clone());

        let executor = self.host_executor(&Host::Localhost)?;
        let pod_id = crate::executor::pod_id_for(&pod_name, &repo_path);
        if executor.status(&pod_id)? != PodStatus::Running {
            return Ok(());
        }
        let serve_port = read_container_server_port(&executor, &pod_id)
            .context("reading server-port file while restoring running pod")?;

        let proxy = block_on(crate::exec_proxy::start_exec_proxy(
            executor.clone(),
            pod_id.clone(),
            serve_port,
        ))
        .context("starting exec proxy")?;
        let container_url = format!("http://127.0.0.1:{}", proxy.port);
        self.exec_proxies
            .lock()
            .unwrap()
            .insert((repo_path.clone(), pod_name.0.clone()), proxy);

        let tunnel = block_on(crate::tunnel::start_tunnel(
            &executor,
            &pod_id,
            &format!("127.0.0.1:{}", self.localhost_server_port),
        ))
        .context("starting docker tunnel")?;
        self.docker_tunnels
            .lock()
            .unwrap()
            .insert((repo_path.clone(), pod_name.0.clone()), tunnel);

        self.pod_events.start(
            repo_path,
            pod_name.0,
            container_url,
            pod.token.clone(),
            Host::Localhost,
        );

        Ok(())
    }

    /// Reference to the daemon's screen-session registry, exposed
    /// for the codex WS handler.
    pub(crate) fn pty_sessions(&self) -> crate::pty_session::PtySessions {
        self.pty_sessions.clone()
    }

    /// Look up a pod's bearer token by (repo_path, pod_name).
    /// Returns Ok(None) if the pod is not in the database.
    pub(crate) fn pod_token(&self, repo_path: &Path, pod_name: &str) -> Result<Option<String>> {
        let conn = self.db.lock().unwrap();
        let record = db::get_pod(&conn, repo_path, pod_name)?;
        Ok(record.map(|r| r.token))
    }

    /// Build a `http://127.0.0.1:N` URL to reach the pod server via
    /// the running exec proxy for this pod.  Returns None if no proxy
    /// is registered (pod was never launched in this daemon's
    /// lifetime).
    pub(crate) fn pod_container_url(&self, repo_path: &Path, pod_name: &str) -> Option<String> {
        let key = (repo_path.to_path_buf(), pod_name.to_string());
        let proxies = self.exec_proxies.lock().unwrap();
        proxies
            .get(&key)
            .map(|h| format!("http://127.0.0.1:{}", h.port))
    }

    fn cleanup_codex_runtime(&self, repo_path: &Path, pod_name: &str) {
        let key = (repo_path.to_path_buf(), pod_name.to_string());
        self.codex_proxies.lock().unwrap().remove(&key);

        let session_name = crate::codex::codex_session_name(repo_path, pod_name);
        if let Err(e) = block_on(self.pty_sessions.terminate(&session_name)) {
            error!("{e:#}");
        }
    }

    /// Idempotently bind a per-pod loopback TCP listener that
    /// proxies WebSocket traffic to the pod server's /codex endpoint.
    /// Returns the listener port and local capability token.  The codex
    /// TUI process gets the token in its environment and dials the port
    /// via `--remote ws://127.0.0.1:{port}`.
    pub(crate) fn ensure_codex_proxy(
        &self,
        repo_path: &Path,
        pod_name: &str,
        container_url: String,
        container_token: String,
    ) -> Result<CodexProxyEndpoint> {
        let key = (repo_path.to_path_buf(), pod_name.to_string());

        {
            let proxies = self.codex_proxies.lock().unwrap();
            if let Some(handle) = proxies.get(&key) {
                return Ok(CodexProxyEndpoint {
                    port: handle.port,
                    token: handle.token.clone(),
                });
            }
        }

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").context("binding codex proxy listener")?;
        listener
            .set_nonblocking(true)
            .context("setting nonblocking")?;
        let port = listener.local_addr()?.port();

        let tokio_listener =
            tokio::net::TcpListener::from_std(listener).context("converting to tokio listener")?;

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let token = generate_codex_proxy_token();
        tokio::task::spawn(crate::codex::run_codex_proxy(
            tokio_listener,
            container_url,
            container_token,
            token.clone(),
            ready_tx,
            cancel_rx,
        ));
        // Wait for the accept loop to start so the codex TUI does not
        // race the listener and fail its first connect attempt.
        ready_rx
            .recv()
            .context("waiting for codex proxy accept loop")?;

        self.codex_proxies.lock().unwrap().insert(
            key,
            CodexProxyHandle {
                port,
                token: token.clone(),
                _cancel_tx: cancel_tx,
            },
        );

        Ok(CodexProxyEndpoint { port, token })
    }
}

fn generate_codex_proxy_token() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

impl Daemon for DaemonServer {
    type Progress = ServerLaunchProgress;

    fn launch_pod(&self, params: PodLaunchParams) -> Result<ServerLaunchProgress> {
        let (tx, rx) = std::sync::mpsc::channel();
        let this = self.clone();
        let handle = std::thread::spawn(move || this.launch_pod_impl(params, tx));
        Ok(ServerLaunchProgress {
            rx: Some(rx),
            handle: Some(handle),
        })
    }

    fn recreate_pod(&self, params: PodLaunchParams) -> Result<ServerLaunchProgress> {
        let (tx, rx) = std::sync::mpsc::channel();
        let this = self.clone();
        let handle = std::thread::spawn(move || this.recreate_pod_impl(params, tx));
        Ok(ServerLaunchProgress {
            rx: Some(rx),
            handle: Some(handle),
        })
    }

    fn fork_pod(&self, request: ForkPodRequest) -> Result<ServerLaunchProgress> {
        let (tx, rx) = std::sync::mpsc::channel();
        let this = self.clone();
        let handle = std::thread::spawn(move || this.fork_pod_impl(request, tx));
        Ok(ServerLaunchProgress {
            rx: Some(rx),
            handle: Some(handle),
        })
    }

    fn stop_pod(&self, pod_name: PodName, repo_path: PathBuf, wait: bool) -> Result<()> {
        let conn = self.db.lock().unwrap();
        let pod_record = db::get_pod(&conn, &repo_path, &pod_name.0)?;

        // Reject k8s pods up-front so the error names this command's
        // alternative rather than the executor's generic "stop not
        // supported on kubernetes".
        let host = match pod_record.as_ref() {
            Some(record) => serde_json::from_str::<Host>(&record.host)?,
            None => Host::Localhost,
        };
        if let Host::Kubernetes { .. } = &host {
            return Err(anyhow::anyhow!(
                "Kubernetes pods cannot be stopped. \
                 Use 'rumpel delete {}' instead.",
                pod_name.0
            ));
        }
        if let Some(ref record) = pod_record {
            db::update_pod_status(&conn, record.id, db::PodStatus::Stopping)?;
        }
        drop(conn);

        self.pod_events.stop(&repo_path, &pod_name.0);
        self.cleanup_codex_runtime(&repo_path, &pod_name.0);

        let pod_id = crate::executor::pod_id_for(&pod_name, &repo_path);
        let executor = self.host_executor(&host)?;

        if wait {
            let result = executor.stop(&pod_id);
            let conn = self.db.lock().unwrap();
            if let Ok(Some(record)) = db::get_pod(&conn, &repo_path, &pod_name.0) {
                let _ = db::update_pod_status(&conn, record.id, db::PodStatus::Ready);
            }
            result?;
        } else {
            let db = self.db.clone();
            let repo_path = repo_path.clone();
            let pod_name = pod_name.clone();
            std::thread::spawn(move || {
                if let Err(e) = executor.stop(&pod_id) {
                    let name = &pod_name.0;
                    error!("failed to stop pod '{name}': {e}");
                }
                let conn = db.lock().unwrap();
                if let Ok(Some(record)) = db::get_pod(&conn, &repo_path, &pod_name.0) {
                    let _ = db::update_pod_status(&conn, record.id, db::PodStatus::Ready);
                }
            });
        }

        Ok(())
    }

    fn delete_pod(&self, pod_name: PodName, repo_path: PathBuf, wait: bool) -> Result<()> {
        let conn = self.db.lock().unwrap();
        let pod_record = db::get_pod(&conn, &repo_path, &pod_name.0)?;
        if let Some(ref record) = pod_record {
            db::update_pod_status(&conn, record.id, db::PodStatus::Deleting)?;
        }
        let host = match pod_record.as_ref() {
            Some(record) => serde_json::from_str::<Host>(&record.host)?,
            None => Host::Localhost,
        };
        drop(conn);

        self.pod_events.stop(&repo_path, &pod_name.0);
        self.cleanup_codex_runtime(&repo_path, &pod_name.0);

        let pod_id = crate::executor::pod_id_for(&pod_name, &repo_path);
        let is_k8s = matches!(host, Host::Kubernetes { .. });

        // Drop the backend-specific handles up-front so any exec
        // sessions inside the container are cleaned up before we try
        // to remove it.  The docker overlay unmount only finishes
        // once exec sessions have gone away.
        let pod_key = (repo_path.clone(), pod_name.0.clone());
        self.port_forward_proxies.lock().unwrap().remove(&pod_key);
        if is_k8s {
            self.k8s_tunnels.lock().unwrap().remove(&pod_key);
        } else {
            self.docker_tunnels.lock().unwrap().remove(&pod_key);
            self.ssh_agents.lock().unwrap().remove(&pod_key);
            let agent_dir = ssh_agent_dir(&repo_path, &pod_name);
            if agent_dir.exists() {
                if let Err(e) = std::fs::remove_dir_all(&agent_dir) {
                    let dir = agent_dir.display();
                    error!("failed to remove ssh-agent directory {dir}: {e}");
                }
            }
        }
        self.exec_proxies.lock().unwrap().remove(&pod_key);

        let executor = self.host_executor(&host)?;

        // K8s delete is a quick API call, so wait inline.  Docker
        // overlay unmounts are sometimes slow and unreliable, so the
        // non-wait path runs deletion in a background thread with
        // retries and only marks the DB record removed on success.
        if is_k8s || wait {
            let result = executor.delete(&pod_id);
            if result.is_ok() {
                cleanup_pod_refs(&repo_path, &pod_name);
                let conn = self.db.lock().unwrap();
                db::delete_pod(&conn, &repo_path, &pod_name.0)?;
            }
            result?;
        } else {
            let db = self.db.clone();
            let repo_path = repo_path.clone();
            let pod_name = pod_name.clone();
            std::thread::spawn(move || {
                let delays_secs = [0, 10, 60];
                for (attempt, &delay) in delays_secs.iter().enumerate() {
                    if delay > 0 {
                        std::thread::sleep(std::time::Duration::from_secs(delay));
                    }
                    match executor.delete(&pod_id) {
                        Ok(()) => {
                            cleanup_pod_refs(&repo_path, &pod_name);
                            let conn = db.lock().unwrap();
                            let _ = db::delete_pod(&conn, &repo_path, &pod_name.0);
                            return;
                        }
                        Err(e) => {
                            error!(
                                "delete attempt {} for pod '{}' failed: {}",
                                attempt + 1,
                                pod_name.0,
                                e
                            );
                        }
                    }
                }
                let name = &pod_name.0;
                error!("all delete attempts failed for pod '{name}'");
                let conn = db.lock().unwrap();
                if let Ok(Some(record)) = db::get_pod(&conn, &repo_path, &pod_name.0) {
                    let _ = db::update_pod_status(&conn, record.id, db::PodStatus::DeleteFailed);
                }
            });
        }

        Ok(())
    }

    fn list_pods(&self, repo_path: PathBuf) -> Result<Vec<PodInfo>> {
        use crate::executor::PodBackendInfo;

        // Get pods from database (includes remote pods)
        let conn = self.db.lock().unwrap();
        let db_pods = db::list_pods(&conn, &repo_path)?;
        drop(conn); // Release lock before calling Docker API

        // Get container status from local Docker.  Ignore connect
        // failures here: a running daemon with no local docker still
        // wants to report on its remote pods.
        let local_container_status: HashMap<String, PodBackendInfo> = self
            .host_executor(&Host::Localhost)
            .and_then(|e| e.list_by_repo(&repo_path))
            .unwrap_or_default();

        // Collect unique remote hosts and check for existing connections
        let mut remote_status_maps: HashMap<String, Option<HashMap<String, PodBackendInfo>>> =
            HashMap::new();
        for pod in &db_pods {
            let host = serde_json::from_str::<Host>(&pod.host).ok();
            match &host {
                Some(h @ Host::Kubernetes { .. }) => {
                    // Cache per kubernetes host to avoid N API calls
                    // when many pods share a cluster/namespace.
                    if !remote_status_maps.contains_key(&pod.host) {
                        let status_map = self
                            .host_executor(h)
                            .and_then(|e| e.list_by_repo(&repo_path))
                            .ok();
                        remote_status_maps.insert(pod.host.clone(), status_map);
                    }
                }
                Some(h @ Host::Ssh { .. }) => {
                    if !remote_status_maps.contains_key(&pod.host) {
                        // Only probe if a connection already exists
                        // and is currently up; do not implicitly
                        // open a fresh SSH connection from `list`.
                        let status_map = self
                            .host_connections
                            .get(h)
                            .filter(|c| c.is_connected())
                            .and_then(|_| {
                                self.host_executor(h)
                                    .and_then(|e| e.list_by_repo(&repo_path))
                                    .ok()
                            });
                        remote_status_maps.insert(pod.host.clone(), status_map);
                    }
                }
                Some(Host::Localhost) | None => {}
            }
        }

        // Build combined list with status from the backend where available
        let mut pods = Vec::new();
        for pod in db_pods {
            let host = serde_json::from_str::<Host>(&pod.host).ok();
            let is_remote = host.as_ref().is_some_and(|h| h.is_remote());
            let is_k8s = host
                .as_ref()
                .is_some_and(|h| matches!(h, Host::Kubernetes { .. }));

            let (status, container_id) = {
                let container_info = if is_remote || is_k8s {
                    remote_status_maps
                        .get(&pod.host)
                        .and_then(|m| m.as_ref())
                        .and_then(|status_map| status_map.get(&pod.name))
                } else {
                    local_container_status.get(&pod.name)
                };

                let status = match pod.status {
                    db::PodStatus::Stopping => PodStatus::Stopping,
                    db::PodStatus::Deleting => PodStatus::Deleting,
                    db::PodStatus::DeleteFailed => PodStatus::Broken,
                    _ => match container_info {
                        Some(info) => info.status.clone(),
                        None => {
                            if is_remote
                                && remote_status_maps
                                    .get(&pod.host)
                                    .is_none_or(|m| m.is_none())
                            {
                                PodStatus::Disconnected
                            } else {
                                match pod.status {
                                    db::PodStatus::Ready => PodStatus::Gone,
                                    db::PodStatus::Initializing | db::PodStatus::Error => {
                                        PodStatus::Stopped
                                    }
                                    db::PodStatus::Stopping
                                    | db::PodStatus::Deleting
                                    | db::PodStatus::DeleteFailed => {
                                        unreachable!()
                                    }
                                }
                            }
                        }
                    },
                };
                let container_id = container_info.map(|info| info.container_id.clone());
                (status, container_id)
            };

            // Compute git status on the local machine by comparing HEAD to rumpelpod/<pod_name>
            let git_info = compute_git_info(&repo_path, &pod.name);

            // Display using Host::Display to normalize the format
            // (e.g. strip default port 22 from old DB entries).
            let display_host = host
                .map(|h| h.to_string())
                .unwrap_or_else(|| pod.host.clone());

            let (claude_state, codex_state) = if status == PodStatus::Running {
                (
                    self.pod_events.claude_state(&repo_path, &pod.name),
                    self.pod_events.codex_state(&repo_path, &pod.name),
                )
            } else {
                (None, None)
            };

            pods.push(PodInfo {
                name: pod.name,
                status,
                created: pod.created_at.format("%Y-%m-%d %H:%M").to_string(),
                host: display_host,
                repo_state: git_info.as_ref().map(|g| g.repo_state.clone()),
                container_id,
                last_commit_time: git_info.map(|g| g.last_commit_time),
                claude_state,
                codex_state,
            });
        }

        Ok(pods)
    }

    fn delete_all_pods(&self) -> Result<u32> {
        let conn = self.db.lock().unwrap();
        let all_pods = db::list_all_pods(&conn)?;
        drop(conn);

        let mut deleted = 0u32;
        for pod in &all_pods {
            let repo_path = Path::new(&pod.repo_path);
            let pod_name = &pod.name;
            let host = match serde_json::from_str::<Host>(&pod.host) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("warning: pod '{pod_name}' has invalid host record: {e}");
                    continue;
                }
            };
            let logical = PodName(pod_name.to_string());
            let pod_id = crate::executor::pod_id_for(&logical, repo_path);
            let result = self.host_executor(&host).and_then(|e| e.delete(&pod_id));
            match result {
                Ok(()) => deleted += 1,
                Err(e) => eprintln!("warning: failed to delete pod '{pod_name}': {e}"),
            }
        }

        Ok(deleted)
    }

    fn list_ports(&self, pod_name: PodName, repo_path: PathBuf) -> Result<Vec<PortInfo>> {
        let conn = self.db.lock().unwrap();
        let pod_rec = db::get_pod(&conn, &repo_path, &pod_name.0)?.context("pod not found")?;

        let ports = db::list_forwarded_ports(&conn, pod_rec.id)?;
        Ok(ports
            .into_iter()
            .map(|p| PortInfo {
                container_port: p.container_port,
                local_port: p.local_port,
                label: p.label,
            })
            .collect())
    }

    fn add_forwarded_port(&self, request: AddForwardedPortRequest) -> Result<PortInfo> {
        let AddForwardedPortRequest {
            pod_name,
            repo_path,
            container_port,
            local_port,
            label,
        } = request;

        let (db_pod_id, host) = {
            let conn = self.db.lock().unwrap();
            let pod_rec = db::get_pod(&conn, &repo_path, &pod_name.0)?
                .with_context(|| format!("pod '{}' not found", pod_name.0))?;
            let existing = db::list_forwarded_ports(&conn, pod_rec.id)?;
            if let Some(dup) = existing.iter().find(|p| p.container_port == container_port) {
                let local = dup.local_port;
                let name = &pod_name.0;
                return Err(anyhow::anyhow!(
                    "container port {container_port} is already forwarded on pod '{name}' (local port {local})"
                ));
            }
            let host: Host =
                serde_json::from_str(&pod_rec.host).context("parsing stored host for pod")?;
            (pod_rec.id, host)
        };

        let executor = self.host_executor(&host)?;
        let exec_pod_id = crate::executor::pod_id_for(&pod_name, &repo_path);
        let status = executor.status(&exec_pod_id)?;
        if status != PodStatus::Running {
            let name = &pod_name.0;
            return Err(anyhow::anyhow!(
                "pod '{name}' is not running (status: {status:?})"
            ));
        }

        // Reserve every host port already promised to any pod so we
        // never hand out a duplicate, even when the saved listener is
        // currently down.
        let allocated: std::collections::HashSet<u16> = {
            let conn = self.db.lock().unwrap();
            db::get_all_allocated_local_ports(&conn)?
                .into_iter()
                .collect()
        };

        let listener = match local_port {
            Some(requested) => {
                if allocated.contains(&requested) {
                    return Err(anyhow::anyhow!(
                        "local port {requested} is already used by another forward"
                    ));
                }
                block_on(tokio::net::TcpListener::bind(format!(
                    "127.0.0.1:{requested}"
                )))
                .with_context(|| format!("binding local port {requested}"))?
            }
            None => block_on(bind_near(container_port, &allocated))
                .context("binding host listener for forward-port")?,
        };
        let actual_local_port = listener.local_addr()?.port();

        {
            let conn = self.db.lock().unwrap();
            db::insert_forwarded_port(&conn, db_pod_id, container_port, actual_local_port, &label)?;
        }

        let handle = crate::exec_proxy::start_exec_proxy_on_listener(
            listener,
            executor,
            exec_pod_id,
            container_port,
        )?;
        self.port_forward_proxies
            .lock()
            .unwrap()
            .entry((repo_path, pod_name.0))
            .or_default()
            .push(handle);

        Ok(PortInfo {
            container_port,
            local_port: actual_local_port,
            label,
        })
    }

    fn ensure_claude_config(&self, request: EnsureClaudeConfigRequest) -> Result<()> {
        let pod_id = {
            let conn = self.db.lock().unwrap();
            let pod_rec = db::get_pod(&conn, &request.repo_path, &request.pod_name.0)?
                .context("Pod not found")?;
            if db::has_claude_config_copied(&conn, pod_rec.id)? {
                return Ok(());
            }
            pod_rec.id
        };

        let pod = PodClient::new(
            &request.container_url,
            &request.container_token,
            RetryPolicy::UserBlocking,
        )?;

        copy_claude_config_via_pod(
            &pod,
            &request.repo_path,
            &request.container_repo_path,
            request.permission_hook,
            request.copy_sessions,
        )?;

        // Mark as copied only after the full copy succeeds.
        // If this DB write fails, the next invocation will redo the copy,
        // which is fine -- overwriting complete files is idempotent.
        let conn = self.db.lock().unwrap();
        db::mark_claude_config_copied(&conn, pod_id)?;

        Ok(())
    }

    fn ensure_pi_config(&self, request: EnsurePiConfigRequest) -> Result<()> {
        let pod_id = {
            let conn = self.db.lock().unwrap();
            let pod_rec = db::get_pod(&conn, &request.repo_path, &request.pod_name.0)?
                .context("Pod not found")?;
            if db::has_pi_config_copied(&conn, pod_rec.id)? {
                return Ok(());
            }
            pod_rec.id
        };

        let pod = PodClient::new(
            &request.container_url,
            &request.container_token,
            RetryPolicy::UserBlocking,
        )?;

        copy_pi_config_via_pod(&pod, request.trust_workspace)?;

        // Mark as copied only after the full copy succeeds.
        let conn = self.db.lock().unwrap();
        db::mark_pi_config_copied(&conn, pod_id)?;

        Ok(())
    }

    fn ensure_ssh_agent(&self, pod_name: PodName, repo_path: PathBuf) -> Result<PathBuf> {
        // Verify the pod exists.
        let conn = self.db.lock().unwrap();
        db::get_pod(&conn, &repo_path, &pod_name.0)?.context("pod not found")?;
        drop(conn);

        let agent_dir = ssh_agent_dir(&repo_path, &pod_name);
        let sock_path = agent_dir.join("agent.sock");

        let mut agents = self.ssh_agents.lock().unwrap();
        let key = (repo_path.clone(), pod_name.0.clone());
        let need_start = if agents.contains_key(&key) {
            let handle = agents.get_mut(&key).unwrap();
            match handle.child.try_wait() {
                Ok(Some(_)) => {
                    agents.remove(&key);
                    true
                }
                Ok(None) => false,
                Err(e) => {
                    eprintln!("warning: failed to check ssh-agent status: {e}");
                    agents.remove(&key);
                    true
                }
            }
        } else {
            true
        };

        if need_start {
            // Clear any stale socket left by a previous daemon run so
            // ssh-agent can re-bind the same path.
            if sock_path.exists() {
                if let Err(e) = std::fs::remove_file(&sock_path) {
                    let path = sock_path.display();
                    eprintln!("warning: failed to remove stale agent socket {path}: {e}");
                }
            }

            std::fs::create_dir_all(&agent_dir).with_context(|| {
                let dir = agent_dir.display();
                format!("creating ssh-agent directory {dir}")
            })?;

            let mut child = Command::new("ssh-agent")
                .args(["-D", "-a"])
                .arg(&sock_path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("failed to start ssh-agent")?;

            // ssh-agent creates the socket asynchronously.  The only way
            // this loop can fail is the agent exiting before it binds,
            // so surface its stderr when that happens.
            while !sock_path.exists() {
                if let Ok(Some(status)) = child.try_wait() {
                    let stderr = child
                        .stderr
                        .take()
                        .and_then(|mut s| {
                            use std::io::Read;
                            let mut buf = String::new();
                            s.read_to_string(&mut buf).ok()?;
                            Some(buf)
                        })
                        .unwrap_or_default();
                    let stderr = stderr.trim();
                    return Err(anyhow::anyhow!("ssh-agent exited with {status}: {stderr}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            agents.insert(key, SshAgentHandle { child });
        }

        Ok(sock_path)
    }

    fn subscribe_pod_reconnect(
        &self,
        repo_path: &Path,
        pod_name: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<reconnect::ReconnectEvent>> {
        self.pod_events.subscribe(repo_path, pod_name)
    }
}

pub fn run_daemon() -> Result<()> {
    // Initialize database
    let db_path = db::db_path()?;
    let db_conn = db::open_db(&db_path)?;
    let db = Arc::new(Mutex::new(db_conn));

    // Single mpsc channel that every host connection writes
    // Connected/Disconnected events to.  The daemon's reader task
    // drains this channel and reacts on transitions.
    let (host_events_tx, host_events_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_connections = Arc::new(HostConnectionRegistry::new(host_events_tx));

    // Enter the runtime context so UnixListener::bind can register with the reactor
    let _guard = crate::async_runtime::RUNTIME.enter();

    // Create shared state for the git HTTP server.  The server
    // validates tokens by querying the database directly.
    let git_server_state = SharedGitServerState::new(db.clone());

    // When RUMPELPOD_TEST_LLM_OFFLINE is set (to any value), enable
    // the LLM cache proxy on the git HTTP server so containers can
    // route API requests through the tunnel for deterministic caching.
    let llm_cache_proxy = std::env::var("RUMPELPOD_TEST_LLM_OFFLINE").ok().map(|_| {
        let cache_base_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../llm-cache");
        for subdir in ["claude-cli", "codex", "grok"] {
            let dir = cache_base_dir.join(subdir);
            std::fs::create_dir_all(dir.join("response")).expect("create llm-cache response dir");
            std::fs::create_dir_all(dir.join("request")).expect("create llm-cache request dir");
        }
        crate::llm::cache_proxy::LlmCacheProxyState { cache_base_dir }
    });

    // Git HTTP server on localhost, used as the tunnel target.
    // Containers reach it via the exec tunnel, not directly.
    let cache_proxy_enabled = llm_cache_proxy.is_some();
    let localhost_server =
        GitHttpServer::start("127.0.0.1", 0, git_server_state.clone(), llm_cache_proxy)
            .context("starting git HTTP server on localhost")?;

    // In test mode the cache proxy is reachable via the daemon's
    // localhost git HTTP server; write the port to a sibling of the
    // daemon socket so test code (running on the host) can discover
    // it without going through a pod.
    if cache_proxy_enabled {
        if let Ok(socket) = socket_path() {
            if let Some(parent) = socket.parent() {
                let port_path = parent.join("llm-cache-proxy-port");
                if let Err(e) = std::fs::write(&port_path, localhost_server.port.to_string()) {
                    eprintln!("warning: failed to write {}: {e}", port_path.display());
                }
            }
        }
    }

    let mut listenfd = ListenFd::from_env();
    let listener = if let Some(listener) = listenfd
        .take_unix_listener(0)
        .context("Failed to take inherited unix listener")?
    {
        listener.set_nonblocking(true)?;
        UnixListener::from_std(listener)?
    } else {
        let socket = socket_path()?;

        // The fallback socket dir (/tmp/rumpelpod-<uid>/) may not exist yet
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                let parent = parent.display();
                format!("Failed to create {parent}")
            })?;
        }

        // Remove stale socket file if it exists
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        UnixListener::bind(&socket).with_context(|| {
            let socket = socket.display();
            format!("Failed to bind to {socket}")
        })?
    };

    let pod_events = Arc::new(PodEventManager::new(host_connections.clone()));

    let daemon = DaemonServer {
        db: db.clone(),
        localhost_server_port: localhost_server.port,
        host_connections,
        pod_events,
        port_forward_proxies: Arc::new(Mutex::new(HashMap::new())),
        k8s_tunnels: Arc::new(Mutex::new(HashMap::new())),
        docker_tunnels: Arc::new(Mutex::new(HashMap::new())),
        exec_proxies: Arc::new(Mutex::new(HashMap::new())),
        ssh_agents: Arc::new(Mutex::new(HashMap::new())),
        codex_proxies: Arc::new(Mutex::new(HashMap::new())),
        pty_sessions: crate::pty_session::PtySessions::new(),
    };

    // Re-establish connections to pods that were running before we
    // (re)started so that pending pushes land without manual re-entry.
    daemon.restore_running_pods();

    // Keep the server alive for the lifetime of the daemon.
    let _localhost_server = localhost_server;

    // Wrap once and share with the codex WS handler so it can reach
    // the same exec_proxies / db / pty_sessions state as the rest of
    // the daemon.
    let daemon = Arc::new(daemon);

    // Single reader task for host connection events.  See
    // `host_event_reader` for what each event triggers.
    {
        let daemon = daemon.clone();
        crate::async_runtime::RUNTIME.spawn(host_event_reader(daemon, host_events_rx));
    }

    let extra_routes = crate::codex::daemon_routes(daemon.clone());
    protocol::serve_daemon(daemon, listener, extra_routes);
}

/// Drain `HostConnectionEvent`s from the registry and react on
/// transitions: on `Disconnected` the daemon broadcasts
/// `ReconnectEvent::Attempting` to every pod on the host so PTY
/// clients show the reconnect state; on `Connected` it broadcasts
/// `HostConnected` so the same clients know the host came back
/// (per-pod loops then retry the pod URL on their own backoff).
async fn host_event_reader(daemon: Arc<DaemonServer>, mut rx: HostConnectionEventRx) {
    while let Some(event) = rx.recv().await {
        match event {
            HostConnectionEvent::Connected(key) => {
                daemon.pod_events.notify_host_connected(&key);
            }
            HostConnectionEvent::Disconnected(key) => {
                daemon.pod_events.notify_host_disconnected(&key);
            }
            HostConnectionEvent::GaveUp(key) => {
                daemon.host_connections.remove(&key);
                daemon.pod_events.notify_host_disconnected(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_upstream_preserves_host() {
        let set = std::collections::HashSet::new();
        assert_eq!(
            rewrite_upstream(Some("host/master"), "S", "N", &set).as_deref(),
            Some("host/master")
        );
    }

    #[test]
    fn rewrite_upstream_remaps_source_primary() {
        let set = std::collections::HashSet::new();
        assert_eq!(
            rewrite_upstream(Some("rumpelpod/S"), "S", "N", &set).as_deref(),
            Some("rumpelpod/N")
        );
        assert_eq!(
            rewrite_upstream(Some("rumpelpod/feature@S"), "S", "N", &set).as_deref(),
            Some("rumpelpod/feature@N")
        );
    }

    #[test]
    fn rewrite_upstream_preserves_other_pods() {
        let set = std::collections::HashSet::new();
        assert_eq!(
            rewrite_upstream(Some("rumpelpod/other"), "S", "N", &set).as_deref(),
            Some("rumpelpod/other")
        );
        assert_eq!(
            rewrite_upstream(Some("rumpelpod/x@other"), "S", "N", &set).as_deref(),
            Some("rumpelpod/x@other")
        );
    }

    #[test]
    fn rewrite_upstream_local_primary_renamed() {
        let set = std::collections::HashSet::new();
        assert_eq!(
            rewrite_upstream(Some("S"), "S", "N", &set).as_deref(),
            Some("N")
        );
    }

    #[test]
    fn rewrite_upstream_local_secondary_kept_when_present() {
        let mut set = std::collections::HashSet::new();
        set.insert("feature".to_string());
        assert_eq!(
            rewrite_upstream(Some("feature"), "S", "N", &set).as_deref(),
            Some("feature")
        );
    }

    #[test]
    fn rewrite_upstream_local_unknown_dropped() {
        let set = std::collections::HashSet::new();
        assert_eq!(rewrite_upstream(Some("ghost"), "S", "N", &set), None);
    }

    #[test]
    fn local_env_round_trips() {
        let mut env = HashMap::new();
        env.insert("A".to_string(), "1".to_string());
        env.insert("B".to_string(), "two=2".to_string());
        let json = serialize_local_env(&env);
        let back = deserialize_local_env(&json).unwrap();
        assert_eq!(back, env);
    }
}
