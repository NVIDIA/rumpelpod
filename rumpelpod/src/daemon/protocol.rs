// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! RPC protocol between the rumpel CLI and the daemon.
//!
//! Both sides are always the same rumpel binary, so these types do not
//! need serde defaults, deny_unknown_fields, or any other backwards-
//! compatibility machinery.  Treat changes as a regular Rust API: add
//! or remove fields freely and let the compiler catch all call sites.

use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::serve::Listener;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::task::block_in_place;
use tokio_stream::StreamExt;
use url::Url;

use crate::async_runtime::block_on;
use crate::config::Host;
use crate::daemon::reconnect::ReconnectEvent;
use crate::git::GitIdentity;
use crate::image::OutputLine;
use crate::pod::types::{ClaudeState, CodexState};

/// Opaque wrapper for docker image names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image(pub String);

/// Opaque wrapper for container IDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerId(pub String);

/// Result of launching a pod.
#[derive(Debug, Clone)]
pub struct LaunchResult {
    pub container_id: ContainerId,
    /// The Docker socket path to use for localhost Docker. `None` for
    /// SSH and Kubernetes pods, which use native client transports.
    pub docker_socket: Option<std::path::PathBuf>,
    /// The host where the pod is running.
    pub host: Host,
    /// Whether a devcontainer image was built during this launch.
    /// False when the image was already cached or when using a pre-built image.
    pub image_built: bool,
    /// Base URL of the in-container HTTP server.  The port is whatever
    /// container-serve picked at startup (see `port_file::SERVER_PORT_FILE`);
    /// for Docker pods this URL typically points at a host-side exec proxy.
    pub container_url: String,
    /// Bearer token for authenticating requests to the in-container HTTP server.
    pub container_token: String,
    /// Container-side workspace path (resolved `workspaceFolder`, or the
    /// `/workspaces/<basename>` default).  The daemon computes this from
    /// the devcontainer.json it loads itself and returns it so the client
    /// can derive the exec `--workdir` without having to parse the config.
    pub container_repo_path: PathBuf,
}

/// Human-readable pod name to distinguish multiple pods for the same repo.
///
/// Restricted to DNS-1123 labels so a single name works unchanged as
/// both a docker container name and a kubernetes pod name.  The
/// restriction is enforced at the two boundaries that can introduce
/// new names: `PodName::new` (CLI input) and serde deserialization
/// (wire protocol).  Loading already-valid names from storage uses
/// the tuple-struct constructor without re-checking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PodName(pub String);

impl PodName {
    /// Construct a `PodName` from untrusted input, rejecting labels
    /// that aren't DNS-1123.
    pub fn new(s: impl Into<String>) -> Result<Self, String> {
        let s = s.into();
        validate_dns_1123_label(&s).map_err(|e| format!("invalid pod name {s:?}: {e}"))?;
        Ok(Self(s))
    }
}

impl TryFrom<String> for PodName {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::new(s)
    }
}

impl From<PodName> for String {
    fn from(p: PodName) -> String {
        p.0
    }
}

impl std::fmt::Display for PodName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn validate_dns_1123_label(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("empty");
    }
    if s.len() > 63 {
        return Err("too long (>63 chars)");
    }
    let bytes = s.as_bytes();
    let first = bytes[0];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err("must start with [a-z0-9]");
    }
    let last = bytes[bytes.len() - 1];
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return Err("must end with [a-z0-9]");
    }
    for &b in bytes {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if !ok {
            return Err("must match [-a-z0-9]*");
        }
    }
    Ok(())
}

/// Status of a pod container.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PodStatus {
    Running,
    Stopped,
    /// Container no longer exists (was deleted outside of rumpel)
    Gone,
    /// Remote pod where we don't have a connection to check actual status
    Disconnected,
    /// Container is being stopped in the background
    Stopping,
    /// Container is being deleted in the background
    Deleting,
    /// Background deletion failed after all retries
    Broken,
}

/// Information about a pod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodInfo {
    pub name: String,
    pub status: PodStatus,
    pub created: String,
    /// Host where the pod runs: "local" or an SSH URL like "user@host:port".
    pub host: String,
    /// State of the repository in the pod (e.g. "ahead 1, behind 2").
    pub repo_state: Option<String>,
    /// Docker container ID of the pod (short 12-char hex), if known.
    pub container_id: Option<String>,
    /// Committer timestamp (unix seconds) of the tip of the pod's primary branch on the host.
    pub last_commit_time: Option<i64>,
    /// Current Claude Code session state, if known.
    pub claude_state: Option<ClaudeState>,
    /// Current Codex session state, if known.
    pub codex_state: Option<CodexState>,
}

/// Information about a forwarded port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortInfo {
    pub container_port: u16,
    pub local_port: u16,
    pub label: String,
}

/// Everything the daemon needs to launch or recreate a pod.
///
/// The client does not parse devcontainer.json: it only scans the raw
/// file for `${localEnv:...}` references and resolves them from the
/// local environment.  The daemon loads and resolves the config itself
/// from `repo_path`.
#[derive(Debug, Serialize, Deserialize)]
pub struct PodLaunchParams {
    pub pod_name: PodName,
    /// Host-side path to the git repository as seen by the client
    /// (e.g. from libgit2 `Repository::discover`).  The daemon uses
    /// this verbatim as the key for DB lookups and canonicalizes a
    /// local copy only where bind-mount paths need it (macOS symlinks).
    pub repo_path: PathBuf,
    /// The branch currently checked out on the host, if any.
    /// Used to set the upstream of the primary branch in the pod.
    pub host_branch: Option<String>,
    /// Where the pod runs: localhost, a remote SSH host, or Kubernetes.
    pub host: Host,
    /// Git user identity from the host, to be written into the pod's .git/config.
    pub git_identity: Option<GitIdentity>,
    /// Absolute path to the Claude CLI binary on the local machine,
    /// resolved by the client so the daemon does not depend on its own
    /// PATH.
    pub claude_cli_path: Option<PathBuf>,
    /// Absolute path to the Codex CLI binary on the local machine,
    /// resolved by the client for the same reason as `claude_cli_path`.
    pub codex_cli_path: Option<PathBuf>,
    /// Absolute path to the pi CLI binary on the local machine,
    /// resolved by the client for the same reason as `claude_cli_path`.
    pub pi_cli_path: Option<PathBuf>,
    /// Absolute path to the Grok CLI binary on the local machine,
    /// resolved by the client for the same reason as `claude_cli_path`.
    pub grok_cli_path: Option<PathBuf>,
    /// Write /etc/claude-code/CLAUDE.md with a rumpelpod environment description
    /// into the prepared image.
    pub inject_system_prompt: bool,
    /// Path of the description file for merge commit messages (None = disabled).
    /// Included in the system prompt so the agent knows where to write it.
    pub description_file: Option<String>,
    /// Host-side environment variables referenced by `${localEnv:...}` in the
    /// raw devcontainer.json.  Collected by the client (which has access to the
    /// host env) and forwarded to the daemon so it can substitute
    /// `${localEnv:...}` without touching its own process environment.
    pub local_env_vars: HashMap<String, String>,
    /// `SSH_AUTH_SOCK` from the client's environment, if set.  The daemon
    /// forwards it verbatim as `SSH_AUTH_SOCK` on every `docker buildx
    /// build` it spawns; buildx itself decides whether to use it (e.g.
    /// when the Dockerfile or `build.options` asks for `--ssh=default`).
    /// `None` leaves the daemon's own `SSH_AUTH_SOCK` in place unchanged.
    pub ssh_auth_sock: Option<PathBuf>,
}

/// Request body for `rumpel fork`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ForkPodRequest {
    pub source: String,
    pub new_name: String,
    pub repo_path: PathBuf,
    /// Skip the interactive confirmation when the source's claude or
    /// codex is mid-turn.  False errors out instead of prompting; the
    /// CLI handles the TTY prompt before forwarding the request.
    pub allow_processing: bool,
}

/// Response body for launch/recreate pod endpoints.
#[derive(Debug, Serialize, Deserialize)]
pub struct PodLaunchResponse {
    pub container_id: ContainerId,
    /// The Docker socket path to use for localhost Docker. `None` for
    /// SSH and Kubernetes pods.
    pub docker_socket: Option<PathBuf>,
    /// The host where the pod is running.
    pub host: Host,
    /// Whether a devcontainer image was built during this launch.
    pub image_built: bool,
    pub container_url: String,
    pub container_token: String,
    /// Container-side workspace path computed by the daemon from the
    /// fully-resolved devcontainer.json.
    pub container_repo_path: PathBuf,
}

/// Request body for stop_pod endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct StopPodRequest {
    pod_name: PodName,
    repo_path: PathBuf,
    wait: bool,
}

/// Response body for stop_pod endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct StopPodResponse {
    stopped: bool,
}

/// Request body for delete_pod endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct DeletePodRequest {
    pod_name: PodName,
    repo_path: PathBuf,
    wait: bool,
}

/// Response body for delete_pod endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct DeletePodResponse {
    deleted: bool,
}

/// Response body for delete_all_pods endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct DeleteAllPodsResponse {
    deleted: u32,
}

/// Request body for list_pods endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct ListPodsRequest {
    repo_path: PathBuf,
}

/// Response body for list_pods endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct ListPodsResponse {
    pods: Vec<PodInfo>,
}

/// Request body for list_ports endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct ListPortsRequest {
    pod_name: PodName,
    repo_path: PathBuf,
}

/// Response body for list_ports endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct ListPortsResponse {
    ports: Vec<PortInfo>,
}

/// Request body for add_forwarded_port endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct AddForwardedPortRequest {
    pub pod_name: PodName,
    pub repo_path: PathBuf,
    pub container_port: u16,
    /// If `Some`, the daemon attempts to bind exactly this host port and
    /// errors out if it is unavailable.  `None` lets the daemon pick a
    /// free port near `container_port`, matching devcontainer
    /// `forwardPorts` behavior.
    pub local_port: Option<u16>,
    pub label: String,
}

/// Request body for the ensure_ssh_agent endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct EnsureSshAgentRequest {
    pod_name: PodName,
    repo_path: PathBuf,
}

/// Response body for the ensure_ssh_agent endpoint.
#[derive(Debug, Serialize, Deserialize)]
struct EnsureSshAgentResponse {
    socket_path: PathBuf,
}

/// Request body for ensure_claude_config endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct EnsureClaudeConfigRequest {
    pub pod_name: PodName,
    pub repo_path: PathBuf,
    pub container_repo_path: PathBuf,
    pub container_id: ContainerId,
    /// The Docker socket path for localhost Docker. Unused for SSH
    /// and Kubernetes pods.
    pub docker_socket: Option<PathBuf>,
    pub container_url: String,
    pub container_token: String,
    /// Install a PermissionRequest hook that auto-approves all tool use
    /// instead of passing --dangerously-skip-permissions to the CLI.
    pub permission_hook: bool,
    /// Copy the per-project session JSONLs from
    /// ~/.claude/projects/<encoded-cwd>/ into the pod so `claude
    /// --resume <uuid>` can pick up sessions started on the host.
    /// Driven by the user-facing `claude.sessions` config knob.
    pub copy_sessions: bool,
}

/// Request body for ensure_pi_config endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct EnsurePiConfigRequest {
    pub pod_name: PodName,
    pub repo_path: PathBuf,
    pub container_id: ContainerId,
    pub container_url: String,
    pub container_token: String,
    /// Force `defaultProjectTrust: "always"` into the copied
    /// settings.json so pi's TUI does not block on the project-trust
    /// prompt.  Driven by the user-facing `pi.trustWorkspace` knob.
    pub trust_workspace: bool,
}

/// Request body for the pod reconnect-events SSE endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct PodReconnectRequest {
    pub repo_path: PathBuf,
    pub pod_name: String,
}

/// Error response body.
#[derive(Debug, Serialize, Deserialize)]
struct ErrorResponse {
    error: String,
}

/// Progress handle for a launch/recreate operation.
///
/// Yields build-output lines via `Iterator::next()` and returns the final
/// result from `finish()`.  Implementations exist for the server side
/// (thread-backed), the client side (SSE-backed), and tests (immediate).
pub trait LaunchProgress: Iterator<Item = OutputLine> + Send {
    /// Drain remaining output and return the final result.
    fn finish(self) -> Result<LaunchResult>;
}

/// Trivial `LaunchProgress` that yields no build output -- just wraps a
/// pre-computed result.  Used by mocks and tests.
pub struct ImmediateLaunchProgress(Option<Result<LaunchResult>>);

impl ImmediateLaunchProgress {
    pub fn new(result: Result<LaunchResult>) -> Self {
        Self(Some(result))
    }
}

impl Iterator for ImmediateLaunchProgress {
    type Item = OutputLine;
    fn next(&mut self) -> Option<OutputLine> {
        None
    }
}

impl LaunchProgress for ImmediateLaunchProgress {
    fn finish(mut self) -> Result<LaunchResult> {
        self.0
            .take()
            .expect("finish() called twice on ImmediateLaunchProgress")
    }
}

pub trait Daemon: Send + Sync + 'static {
    type Progress: LaunchProgress;

    // PUT /pod
    fn launch_pod(&self, params: PodLaunchParams) -> Result<Self::Progress>;

    // POST /pod/recreate
    fn recreate_pod(&self, params: PodLaunchParams) -> Result<Self::Progress>;

    // POST /pod/fork
    // Spawn a new pod by cloning an existing one's image, devcontainer,
    // local_env, branches, agent state, and dirty working tree.
    fn fork_pod(&self, request: ForkPodRequest) -> Result<Self::Progress>;

    // POST /pod/stop
    // Stops a pod container without removing it.  When wait is true, blocks
    // until the container is fully stopped; otherwise returns immediately.
    fn stop_pod(&self, pod_name: PodName, repo_path: PathBuf, wait: bool) -> Result<()>;

    // DELETE /pod
    // Stops and removes a pod container.  When wait is true, blocks until the
    // container is fully removed; otherwise returns immediately.
    fn delete_pod(&self, pod_name: PodName, repo_path: PathBuf, wait: bool) -> Result<()>;

    // GET /pod
    // Lists all pods for a given repository.
    fn list_pods(&self, repo_path: PathBuf) -> Result<Vec<PodInfo>>;

    // POST /pods/delete-all
    // Nukes all containers/pods across all repos.  Only triggers
    // removal (docker rm / kubectl delete); does not wait for
    // completion or clean up the database.  For test cleanup only --
    // the daemon is expected to exit shortly after this call.
    fn delete_all_pods(&self) -> Result<u32>;

    // GET /pod/ports
    fn list_ports(&self, pod_name: PodName, repo_path: PathBuf) -> Result<Vec<PortInfo>>;

    // POST /pod/ports
    // Add a one-off forward on top of devcontainer `forwardPorts`.
    // The forward is recorded in the database and re-bound on
    // reconnect, just like a devcontainer-declared one.
    fn add_forwarded_port(&self, request: AddForwardedPortRequest) -> Result<PortInfo>;

    // PUT /pod/claude-config
    // Ensure Claude Code config files are present in the container.
    // Idempotent: skips the copy if it has already been done for this pod.
    fn ensure_claude_config(&self, request: EnsureClaudeConfigRequest) -> Result<()>;

    // PUT /pod/pi-config
    // Ensure pi auth/config files are present in the container.
    // Idempotent: skips the copy if it has already been done for this pod.
    fn ensure_pi_config(&self, request: EnsurePiConfigRequest) -> Result<()>;

    // POST /pod/ssh-agent
    // Ensure the pod's host-side ssh-agent is running and return the
    // path to its Unix socket.  The caller invokes `ssh-add` locally
    // with `SSH_AUTH_SOCK` set to this path.
    fn ensure_ssh_agent(&self, pod_name: PodName, repo_path: PathBuf) -> Result<PathBuf>;

    // POST /pod/reconnect-events
    // Subscribe to reconnection events for a pod.
    // Returns None if no event listener is active for the pod.
    fn subscribe_pod_reconnect(
        &self,
        _repo_path: &Path,
        _pod_name: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<ReconnectEvent>> {
        None
    }
}

pub struct DaemonClient {
    client: reqwest::blocking::Client,
    /// Base URL for HTTP requests. For Unix sockets, use any valid URL
    /// (the host is ignored since the socket path is set on the client).
    url: Url,
}

impl DaemonClient {
    /// Create a client that connects via Unix domain socket.
    pub fn new_unix(socket_path: &Path) -> Self {
        Self::new_unix_with_timeout(socket_path, None)
    }

    /// Variant with a request timeout, intended for shell completion so a
    /// stalled daemon never hangs the user's prompt.
    pub fn new_unix_with_timeout(socket_path: &Path, timeout: Option<Duration>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .unix_socket(socket_path)
            .timeout(timeout)
            .build()
            .expect("failed to build reqwest client");
        // URL host is ignored for Unix sockets, but we need a valid URL
        let url = Url::parse("http://localhost").unwrap();
        Self { client, url }
    }
}

/// SSE-backed `LaunchProgress` for the HTTP client side.
///
/// Parses `event: build_stdout` / `event: build_stderr` / `event: result` /
/// `event: error` lines from a `text/event-stream` response.
pub struct ClientLaunchProgress {
    lines: std::io::Lines<BufReader<reqwest::blocking::Response>>,
    result: Option<Result<LaunchResult>>,
}

impl ClientLaunchProgress {
    fn new(response: reqwest::blocking::Response) -> Self {
        Self {
            lines: BufReader::new(response).lines(),
            result: None,
        }
    }
}

impl Iterator for ClientLaunchProgress {
    type Item = OutputLine;

    fn next(&mut self) -> Option<OutputLine> {
        // SSE format: "event: <type>\ndata: <json>\n\n"
        // We need to read event + data line pairs.
        loop {
            let line = match self.lines.next()? {
                Ok(l) => l,
                Err(e) => {
                    self.result = Some(Err(anyhow::anyhow!("failed to read response stream: {e}")));
                    return None;
                }
            };

            // Skip blank lines (SSE record separator)
            if line.is_empty() {
                continue;
            }

            let event_type = match line.strip_prefix("event: ") {
                Some(t) => t.to_string(),
                None => continue,
            };

            // Read the "data: ..." line
            let data_line = match self.lines.next() {
                Some(Ok(l)) => l,
                Some(Err(e)) => {
                    self.result = Some(Err(anyhow::anyhow!("failed to read data line: {e}")));
                    return None;
                }
                None => {
                    self.result = Some(Err(anyhow::anyhow!(
                        "stream ended mid-event (no data line)"
                    )));
                    return None;
                }
            };

            let data = match data_line.strip_prefix("data: ") {
                Some(d) => d,
                None => {
                    self.result = Some(Err(anyhow::anyhow!(
                        "expected 'data: ' line, got: {data_line}"
                    )));
                    return None;
                }
            };

            match event_type.as_str() {
                "build_stdout" | "build_stderr" => {
                    // data is a JSON-encoded string
                    match serde_json::from_str::<String>(data) {
                        Ok(s) => {
                            if event_type == "build_stdout" {
                                return Some(OutputLine::Stdout(s));
                            } else {
                                return Some(OutputLine::Stderr(s));
                            }
                        }
                        Err(e) => {
                            self.result = Some(Err(anyhow::anyhow!(
                                "failed to parse build output data: {e}"
                            )));
                            return None;
                        }
                    }
                }
                "result" => {
                    match serde_json::from_str::<PodLaunchResponse>(data) {
                        Ok(body) => {
                            self.result = Some(Ok(LaunchResult {
                                container_id: body.container_id,
                                docker_socket: body.docker_socket,
                                host: body.host,
                                image_built: body.image_built,
                                container_url: body.container_url,
                                container_token: body.container_token,
                                container_repo_path: body.container_repo_path,
                            }));
                        }
                        Err(e) => {
                            self.result =
                                Some(Err(anyhow::anyhow!("failed to parse result data: {e}")));
                        }
                    }
                    return None;
                }
                "error" => {
                    match serde_json::from_str::<ErrorResponse>(data) {
                        Ok(err) => {
                            let msg = &err.error;
                            self.result = Some(Err(anyhow::anyhow!("server error: {msg}")));
                        }
                        Err(e) => {
                            self.result =
                                Some(Err(anyhow::anyhow!("failed to parse error data: {e}")));
                        }
                    }
                    return None;
                }
                other => {
                    self.result = Some(Err(anyhow::anyhow!("unknown SSE event type: {other}")));
                    return None;
                }
            }
        }
    }
}

impl LaunchProgress for ClientLaunchProgress {
    fn finish(mut self) -> Result<LaunchResult> {
        // Drain any remaining build output
        while self.next().is_some() {}

        self.result.unwrap_or_else(|| {
            Err(anyhow::anyhow!(
                "server closed stream without sending a result"
            ))
        })
    }
}

impl Daemon for DaemonClient {
    type Progress = ClientLaunchProgress;

    fn launch_pod(&self, params: PodLaunchParams) -> Result<ClientLaunchProgress> {
        let url = self.url.join("/pod")?;

        let response = self
            .client
            .put(url)
            .json(&params)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        Ok(ClientLaunchProgress::new(response))
    }

    fn recreate_pod(&self, params: PodLaunchParams) -> Result<ClientLaunchProgress> {
        let url = self.url.join("/pod/recreate")?;

        let response = self
            .client
            .post(url)
            .json(&params)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        Ok(ClientLaunchProgress::new(response))
    }

    fn fork_pod(&self, request: ForkPodRequest) -> Result<ClientLaunchProgress> {
        let url = self.url.join("/pod/fork")?;

        let response = self
            .client
            .post(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        Ok(ClientLaunchProgress::new(response))
    }

    fn stop_pod(&self, pod_name: PodName, repo_path: PathBuf, wait: bool) -> Result<()> {
        let url = self.url.join("/pod/stop")?;
        let description = format!("stopping pod '{}'", pod_name.0);
        let request = StopPodRequest {
            pod_name,
            repo_path,
            wait,
        };

        let response = self
            .client
            .post(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        let _: StopPodResponse = read_sse_result(response, &description)?;
        Ok(())
    }

    fn delete_pod(&self, pod_name: PodName, repo_path: PathBuf, wait: bool) -> Result<()> {
        let url = self.url.join("/pod")?;
        let description = format!("deleting pod '{}'", pod_name.0);
        let request = DeletePodRequest {
            pod_name,
            repo_path,
            wait,
        };

        let response = self
            .client
            .delete(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        let _: DeletePodResponse = read_sse_result(response, &description)?;
        Ok(())
    }

    fn list_pods(&self, repo_path: PathBuf) -> Result<Vec<PodInfo>> {
        let url = self.url.join("/pod")?;
        let request = ListPodsRequest { repo_path };

        let response = self
            .client
            .get(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        let body: ListPodsResponse = read_sse_result(response, "listing pods")?;
        Ok(body.pods)
    }

    fn delete_all_pods(&self) -> Result<u32> {
        let url = self.url.join("/pods/delete-all")?;

        let response = self
            .client
            .post(url)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        let body: DeleteAllPodsResponse = read_sse_result(response, "deleting all pods")?;
        Ok(body.deleted)
    }

    fn list_ports(&self, pod_name: PodName, repo_path: PathBuf) -> Result<Vec<PortInfo>> {
        let url = self.url.join("/pod/ports")?;
        let request = ListPortsRequest {
            pod_name,
            repo_path,
        };

        let response = self
            .client
            .get(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        if response.status().is_success() {
            let body: ListPortsResponse = response
                .json()
                .map_err(|e| anyhow::anyhow!("failed to parse response: {e}"))?;
            Ok(body.ports)
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let msg = &error.error;
            Err(anyhow::anyhow!("server error: {msg}"))
        }
    }

    fn add_forwarded_port(&self, request: AddForwardedPortRequest) -> Result<PortInfo> {
        let url = self.url.join("/pod/ports")?;

        let response = self
            .client
            .post(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        if response.status().is_success() {
            response
                .json::<PortInfo>()
                .map_err(|e| anyhow::anyhow!("failed to parse response: {e}"))
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            Err(anyhow::anyhow!("{}", error.error))
        }
    }

    fn ensure_claude_config(&self, request: EnsureClaudeConfigRequest) -> Result<()> {
        let url = self.url.join("/pod/claude-config")?;

        let response = self
            .client
            .put(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        let _: serde_json::Value = read_sse_result(response, "configuring Claude Code")?;
        Ok(())
    }

    fn ensure_pi_config(&self, request: EnsurePiConfigRequest) -> Result<()> {
        let url = self.url.join("/pod/pi-config")?;

        let response = self
            .client
            .put(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("Failed to send request: {e}"))?;

        let _: serde_json::Value = read_sse_result(response, "configuring pi")?;
        Ok(())
    }

    fn ensure_ssh_agent(&self, pod_name: PodName, repo_path: PathBuf) -> Result<PathBuf> {
        let url = self.url.join("/pod/ssh-agent")?;
        let request = EnsureSshAgentRequest {
            pod_name,
            repo_path,
        };

        let response = self
            .client
            .post(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        if response.status().is_success() {
            let body: EnsureSshAgentResponse = response
                .json()
                .map_err(|e| anyhow::anyhow!("failed to parse response: {e}"))?;
            Ok(body.socket_path)
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let msg = &error.error;
            Err(anyhow::anyhow!("server error: {msg}"))
        }
    }
}

impl DaemonClient {
    /// Subscribe to pod-level reconnection events (SSH + pod).
    pub fn pod_reconnect_events(
        &self,
        repo_path: &Path,
        pod_name: &str,
    ) -> Result<ReconnectStream> {
        let url = self.url.join("/pod/reconnect-events")?;
        let request = PodReconnectRequest {
            repo_path: repo_path.to_path_buf(),
            pod_name: pod_name.to_string(),
        };

        let response = self
            .client
            .post(url)
            .json(&request)
            .send()
            .map_err(|e| anyhow::anyhow!("failed to send request: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let msg = &error.error;
            return Err(anyhow::anyhow!(
                "reconnect endpoint returned {status}: {msg}"
            ));
        }

        Ok(ReconnectStream::new(response))
    }
}

/// SSE-backed iterator over `ReconnectEvent`s from the daemon.
pub struct ReconnectStream {
    lines: std::io::Lines<BufReader<reqwest::blocking::Response>>,
}

impl ReconnectStream {
    fn new(response: reqwest::blocking::Response) -> Self {
        Self {
            lines: BufReader::new(response).lines(),
        }
    }
}

impl Iterator for ReconnectStream {
    type Item = Result<ReconnectEvent>;

    fn next(&mut self) -> Option<Result<ReconnectEvent>> {
        loop {
            let line = match self.lines.next()? {
                Ok(l) => l,
                Err(e) => return Some(Err(anyhow::anyhow!("failed to read event stream: {e}"))),
            };

            if line.is_empty() {
                continue;
            }

            let event_type = match line.strip_prefix("event: ") {
                Some(t) => t.to_string(),
                None => continue,
            };

            let data_line = match self.lines.next() {
                Some(Ok(l)) => l,
                Some(Err(e)) => return Some(Err(anyhow::anyhow!("failed to read data line: {e}"))),
                None => {
                    return Some(Err(anyhow::anyhow!(
                        "stream ended mid-event (no data line)"
                    )))
                }
            };

            let data = match data_line.strip_prefix("data: ") {
                Some(d) => d,
                None => {
                    return Some(Err(anyhow::anyhow!(
                        "expected 'data: ' line, got: {data_line}"
                    )))
                }
            };

            match event_type.as_str() {
                "attempting" => return Some(Ok(ReconnectEvent::Attempting)),
                "host_connected" => return Some(Ok(ReconnectEvent::HostConnected)),
                "connected" => return Some(Ok(ReconnectEvent::Connected)),
                "stopped" => return Some(Ok(ReconnectEvent::Stopped)),
                "failed" => {
                    #[derive(Deserialize)]
                    struct FailedData {
                        error: String,
                    }
                    match serde_json::from_str::<FailedData>(data) {
                        Ok(f) => return Some(Ok(ReconnectEvent::Failed { error: f.error })),
                        Err(e) => {
                            return Some(Err(anyhow::anyhow!(
                                "failed to parse failed event data: {e}"
                            )))
                        }
                    }
                }
                other => return Some(Err(anyhow::anyhow!("unknown SSE event type: {other}"))),
            }
        }
    }
}

/// Format a single SSE event with the given type and JSON-encoded data.
fn sse_event(event_type: &str, data: &str) -> String {
    format!("event: {event_type}\ndata: {data}\n\n")
}

/// Build an SSE streaming response for a blocking operation that returns
/// a single result.
///
/// A `SlowGuard` emits periodic `event: log` messages while the
/// operation runs.  The final result is sent as `event: result` (or
/// `event: error` on failure).
fn streaming_result_response<T: Serialize + Send + 'static>(
    description: String,
    op: impl FnOnce() -> Result<T> + Send + 'static,
) -> Response {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<String>(64);

    // SlowGuard sends plain strings; forward them as SSE log events.
    let (guard_tx, mut guard_rx) = tokio::sync::mpsc::channel::<String>(16);
    let log_fwd = event_tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = guard_rx.recv().await {
            let json = serde_json::to_string(&msg).expect("String is always serializable");
            if log_fwd.send(sse_event("log", &json)).await.is_err() {
                break;
            }
        }
    });

    tokio::task::spawn_blocking(move || {
        let _guard = crate::slow_guard::SlowGuard::new(description, guard_tx);
        let result = op();
        drop(_guard);

        let msg = match result {
            Ok(value) => sse_event(
                "result",
                &serde_json::to_string(&value).expect("result type is always serializable"),
            ),
            Err(e) => sse_event(
                "error",
                &serde_json::to_string(&ErrorResponse {
                    error: format!("{e:#}"),
                })
                .expect("ErrorResponse is always serializable"),
            ),
        };
        if event_tx.blocking_send(msg).is_err() {
            eprintln!("client disconnected before result was sent");
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(event_rx)
        .map(Ok::<_, std::convert::Infallible>);

    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .expect("building response never fails")
}

/// Read an SSE stream that carries `event: log`, `event: result`, and
/// `event: error` messages.
///
/// Log messages are forwarded to stderr.  If no events arrive within
/// the silence timeout, a client-side progress message is printed.
/// Returns the deserialized result.
fn read_sse_result<T: serde::de::DeserializeOwned + Send + 'static>(
    response: reqwest::blocking::Response,
    description: &str,
) -> Result<T> {
    use std::sync::mpsc;

    enum SseItem<T> {
        Log(String),
        Result(T),
        Error(String),
    }

    let (tx, rx) = mpsc::channel::<Result<SseItem<T>>>();

    // Reader thread: parse SSE events and send them over the channel.
    // A thread (not an async task) because reqwest::blocking::Response
    // blocks on read and we need the main thread free for timeout logic.
    //
    // If `tx.send()` fails the main thread already dropped the receiver
    // (it got its answer or timed out).  The reader thread just exits.
    std::thread::spawn(move || {
        let send = |item: Result<SseItem<T>>| -> bool { tx.send(item).is_ok() };

        let reader = BufReader::new(response);
        let mut lines = reader.lines();
        loop {
            let line = match lines.next() {
                Some(Ok(l)) => l,
                Some(Err(e)) => {
                    send(Err(anyhow::anyhow!("reading SSE stream: {e}")));
                    return;
                }
                None => {
                    send(Err(anyhow::anyhow!("SSE stream closed without result")));
                    return;
                }
            };

            if line.is_empty() {
                continue;
            }

            let event_type = match line.strip_prefix("event: ") {
                Some(t) => t.to_string(),
                None => continue,
            };

            let data_line = match lines.next() {
                Some(Ok(l)) => l,
                Some(Err(e)) => {
                    send(Err(anyhow::anyhow!("reading SSE data line: {e}")));
                    return;
                }
                None => {
                    send(Err(anyhow::anyhow!("SSE stream ended mid-event")));
                    return;
                }
            };

            let data = match data_line.strip_prefix("data: ") {
                Some(d) => d,
                None => {
                    send(Err(anyhow::anyhow!(
                        "expected 'data: ' line, got: {data_line}"
                    )));
                    return;
                }
            };

            match event_type.as_str() {
                "log" => match serde_json::from_str::<String>(data) {
                    Ok(msg) => {
                        if !send(Ok(SseItem::Log(msg))) {
                            return;
                        }
                    }
                    Err(e) => {
                        send(Err(anyhow::anyhow!("parsing log event: {e}")));
                        return;
                    }
                },
                "result" => {
                    match serde_json::from_str::<T>(data) {
                        Ok(val) => send(Ok(SseItem::Result(val))),
                        Err(e) => send(Err(anyhow::anyhow!("parsing result event: {e}"))),
                    };
                    return;
                }
                "error" => {
                    match serde_json::from_str::<ErrorResponse>(data) {
                        Ok(err) => send(Ok(SseItem::Error(err.error))),
                        Err(e) => send(Err(anyhow::anyhow!("parsing error event: {e}"))),
                    };
                    return;
                }
                other => {
                    send(Err(anyhow::anyhow!("unknown SSE event type: {other}")));
                    return;
                }
            }
        }
    });

    let first_timeout = Duration::from_secs(30);
    let repeat_timeout = Duration::from_secs(120);
    let mut timeout = first_timeout;

    loop {
        match rx.recv_timeout(timeout) {
            Ok(Ok(SseItem::Log(msg))) => {
                eprintln!("{msg}");
                timeout = repeat_timeout;
            }
            Ok(Ok(SseItem::Result(val))) => return Ok(val),
            Ok(Ok(SseItem::Error(msg))) => return Err(anyhow::anyhow!("{msg}")),
            Ok(Err(e)) => return Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                eprintln!("still waiting: {description}");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("SSE stream closed without result"));
            }
        }
    }
}

/// Build an SSE streaming response for launch/recreate endpoints.
///
/// Spawns a blocking task that calls `op()` to get a `LaunchProgress`,
/// iterates build output lines (sending each as `event: build_stdout` or
/// `event: build_stderr`),
/// then calls `finish()` and sends `event: result` or `event: error`.
fn streaming_launch_response<D: Daemon, P: Send + 'static>(
    daemon: Arc<D>,
    params: P,
    op: fn(&D, P) -> Result<D::Progress>,
) -> Response {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<String>(64);

    // SlowGuard covers the period before any build output arrives.
    // Forward plain-text guard messages as SSE log events.
    let (guard_tx, mut guard_rx) = tokio::sync::mpsc::channel::<String>(16);
    let log_fwd = event_tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = guard_rx.recv().await {
            let json = serde_json::to_string(&msg).expect("String is always serializable");
            if log_fwd.send(sse_event("log", &json)).await.is_err() {
                break;
            }
        }
    });

    tokio::task::spawn_blocking(move || {
        let _guard = crate::slow_guard::SlowGuard::new("launching pod...", guard_tx);

        let mut progress = match op(&daemon, params) {
            Ok(p) => p,
            Err(e) => {
                let msg = sse_event(
                    "error",
                    &serde_json::to_string(&ErrorResponse {
                        error: format!("{e:#}"),
                    })
                    .expect("ErrorResponse is always serializable"),
                );
                if event_tx.blocking_send(msg).is_err() {
                    eprintln!("client disconnected before error was sent");
                }
                return;
            }
        };

        // Build output is flowing; the guard is no longer needed.
        drop(_guard);

        // Stream build output lines
        for line in &mut progress {
            let (event_type, text) = match line {
                OutputLine::Stdout(s) => ("build_stdout", s),
                OutputLine::Stderr(s) => ("build_stderr", s),
            };
            let json_str = serde_json::to_string(&text).expect("String is always serializable");
            let msg = sse_event(event_type, &json_str);
            if event_tx.blocking_send(msg).is_err() {
                return;
            }
        }

        // Send final result
        let msg = match progress.finish() {
            Ok(r) => sse_event(
                "result",
                &serde_json::to_string(&PodLaunchResponse {
                    container_id: r.container_id,
                    docker_socket: r.docker_socket,
                    host: r.host,
                    image_built: r.image_built,
                    container_url: r.container_url,
                    container_token: r.container_token,
                    container_repo_path: r.container_repo_path,
                })
                .expect("PodLaunchResponse is always serializable"),
            ),
            Err(e) => sse_event(
                "error",
                &serde_json::to_string(&ErrorResponse {
                    error: format!("{e:#}"),
                })
                .expect("ErrorResponse is always serializable"),
            ),
        };
        if event_tx.blocking_send(msg).is_err() {
            eprintln!("client disconnected before result was sent");
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(event_rx)
        .map(Ok::<_, std::convert::Infallible>);

    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .expect("building response never fails")
}

/// Handler for PUT /pod endpoint.
async fn launch_pod_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(params): Json<PodLaunchParams>,
) -> Response {
    streaming_launch_response(daemon, params, D::launch_pod)
}

/// Handler for POST /pod/recreate endpoint.
async fn recreate_pod_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(params): Json<PodLaunchParams>,
) -> Response {
    streaming_launch_response(daemon, params, D::recreate_pod)
}

/// Handler for POST /pod/fork endpoint.
async fn fork_pod_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<ForkPodRequest>,
) -> Response {
    streaming_launch_response(daemon, request, D::fork_pod)
}

/// Handler for POST /pod/stop endpoint.
async fn stop_pod_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<StopPodRequest>,
) -> Response {
    let name = request.pod_name.0.clone();
    streaming_result_response(format!("stopping pod '{name}'..."), move || {
        daemon.stop_pod(request.pod_name, request.repo_path, request.wait)?;
        Ok(StopPodResponse { stopped: true })
    })
}

/// Handler for DELETE /pod endpoint.
async fn delete_pod_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<DeletePodRequest>,
) -> Response {
    let name = request.pod_name.0.clone();
    streaming_result_response(format!("deleting pod '{name}'..."), move || {
        daemon.delete_pod(request.pod_name, request.repo_path, request.wait)?;
        Ok(DeletePodResponse { deleted: true })
    })
}

/// Handler for POST /pods/delete-all endpoint.
async fn delete_all_pods_handler<D: Daemon>(State(daemon): State<Arc<D>>) -> Response {
    streaming_result_response("deleting all pods...".into(), move || {
        let deleted = daemon.delete_all_pods()?;
        Ok(DeleteAllPodsResponse { deleted })
    })
}

/// Handler for GET /pod endpoint.
async fn list_pods_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<ListPodsRequest>,
) -> Response {
    streaming_result_response("listing pods...".into(), move || {
        let pods = daemon.list_pods(request.repo_path)?;
        Ok(ListPodsResponse { pods })
    })
}

/// Handler for GET /pod/ports endpoint.
async fn list_ports_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<ListPortsRequest>,
) -> Result<Json<ListPortsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let result = block_in_place(|| daemon.list_ports(request.pod_name, request.repo_path));

    match result {
        Ok(ports) => Ok(Json(ListPortsResponse { ports })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("{e:#}"),
            }),
        )),
    }
}

/// Handler for POST /pod/ports endpoint.
async fn add_forwarded_port_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<AddForwardedPortRequest>,
) -> Result<Json<PortInfo>, (StatusCode, Json<ErrorResponse>)> {
    let result = block_in_place(|| daemon.add_forwarded_port(request));

    match result {
        Ok(port) => Ok(Json(port)),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("{e:#}"),
            }),
        )),
    }
}

/// Handler for PUT /pod/claude-config endpoint.
async fn ensure_claude_config_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<EnsureClaudeConfigRequest>,
) -> Response {
    streaming_result_response("configuring Claude Code...".into(), move || {
        daemon.ensure_claude_config(request)?;
        Ok(serde_json::Value::Null)
    })
}

/// Handler for PUT /pod/pi-config endpoint.
async fn ensure_pi_config_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<EnsurePiConfigRequest>,
) -> Response {
    streaming_result_response("Configuring pi...".into(), move || {
        daemon.ensure_pi_config(request)?;
        Ok(serde_json::Value::Null)
    })
}

/// Handler for POST /pod/ssh-agent endpoint.
async fn ensure_ssh_agent_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<EnsureSshAgentRequest>,
) -> Result<Json<EnsureSshAgentResponse>, (StatusCode, Json<ErrorResponse>)> {
    let result = block_in_place(|| daemon.ensure_ssh_agent(request.pod_name, request.repo_path));

    match result {
        Ok(socket_path) => Ok(Json(EnsureSshAgentResponse { socket_path })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("{e:#}"),
            }),
        )),
    }
}

/// Build an SSE response from a broadcast receiver of reconnect events.
fn reconnect_sse_response(rx: tokio::sync::broadcast::Receiver<ReconnectEvent>) -> Response {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<String>(64);

    tokio::spawn(async move {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let is_terminal =
                        matches!(event, ReconnectEvent::Connected | ReconnectEvent::Stopped);
                    let (event_type, data) = match &event {
                        ReconnectEvent::Attempting => ("attempting", "{}".to_string()),
                        ReconnectEvent::HostConnected => ("host_connected", "{}".to_string()),
                        ReconnectEvent::Connected => ("connected", "{}".to_string()),
                        ReconnectEvent::Failed { error } => (
                            "failed",
                            serde_json::to_string(&serde_json::json!({ "error": error }))
                                .expect("json object is always serializable"),
                        ),
                        ReconnectEvent::Stopped => ("stopped", "{}".to_string()),
                    };
                    let msg = sse_event(event_type, &data);
                    if event_tx.send(msg).await.is_err() {
                        break;
                    }
                    if is_terminal {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(event_rx)
        .map(Ok::<_, std::convert::Infallible>);

    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .expect("building response never fails")
}

/// Handler for POST /pod/reconnect-events endpoint.
///
/// When no event listener is active the pod was already stopped (or
/// never started).  Return a single `Stopped` event so the client
/// exits instead of retrying forever.
async fn pod_reconnect_events_handler<D: Daemon>(
    State(daemon): State<Arc<D>>,
    Json(request): Json<PodReconnectRequest>,
) -> Response {
    match daemon.subscribe_pod_reconnect(&request.repo_path, &request.pod_name) {
        Some(rx) => reconnect_sse_response(rx),
        None => {
            let (tx, rx) = tokio::sync::broadcast::channel(1);
            let _ = tx.send(ReconnectEvent::Stopped);
            reconnect_sse_response(rx)
        }
    }
}

/// Serve the daemon using the provided listener.
///
/// `extra` is a stateless router merged into the main one, used to
/// add routes that cannot be expressed via the trait (e.g. the codex
/// WebSocket bridge, which needs concrete `DaemonServer` access).
/// `daemon` is taken as `Arc<D>` so the caller can share the same
/// instance with the extra router's state.
///
/// The listener can be a `tokio::net::TcpListener` or `tokio::net::UnixListener`.
pub fn serve_daemon<D, L>(daemon: Arc<D>, listener: L, extra: Router<()>) -> !
where
    D: Daemon,
    L: Listener,
    L::Addr: Debug + Send,
    L::Io: Send,
{
    serve_daemon_until(daemon, listener, extra, std::future::pending());
    unreachable!("Server shut down unexpectedly")
}

/// Serve the daemon until the shutdown signal completes.
fn serve_daemon_until<D, L, F>(daemon: Arc<D>, listener: L, extra: Router<()>, shutdown: F)
where
    D: Daemon,
    L: Listener,
    L::Addr: Debug + Send,
    L::Io: Send,
    F: Future<Output = ()> + Send + 'static,
{
    let app = Router::new()
        .route("/pod", put(launch_pod_handler::<D>))
        .route("/pod/recreate", post(recreate_pod_handler::<D>))
        .route("/pod/fork", post(fork_pod_handler::<D>))
        .route("/pod/stop", post(stop_pod_handler::<D>))
        .route("/pod", delete(delete_pod_handler::<D>))
        .route("/pod", get(list_pods_handler::<D>))
        .route("/pods/delete-all", post(delete_all_pods_handler::<D>))
        .route(
            "/pod/ports",
            get(list_ports_handler::<D>).post(add_forwarded_port_handler::<D>),
        )
        .route("/pod/claude-config", put(ensure_claude_config_handler::<D>))
        .route("/pod/pi-config", put(ensure_pi_config_handler::<D>))
        .route("/pod/ssh-agent", post(ensure_ssh_agent_handler::<D>))
        .route(
            "/pod/reconnect-events",
            post(pod_reconnect_events_handler::<D>),
        )
        .with_state(daemon);

    let app = app.merge(extra);

    block_on(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
            .unwrap();
    });
}
