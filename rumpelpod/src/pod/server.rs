// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! HTTP server that runs inside containers as the devcontainer user.
//!
//! Started via `rumpel container-serve` after the binary is copied into the container.
//! Binds a 127.0.0.1 ephemeral port and writes it to
//! `/opt/rumpelpod/server-port` for consumers inside the container.
//! Implements filesystem, git, environment, and command execution
//! operations in Rust instead of composing shell scripts via docker exec.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::git_setup::{self, GitSetupRequest, GitSetupSubmodulesRequest};
use super::types::*;
use crate::async_runtime::block_on;
use crate::command_ext::CommandExt;

pub const TOKEN_FILE: &str = "/opt/rumpelpod/server-token";
/// In-container path for the SSH agent socket served by the relay.
pub const SSH_AGENT_SOCK_PATH: &str = "/tmp/rumpelpod-ssh-agent/agent.sock";

fn is_websocket_upgrade(headers: &axum::http::HeaderMap) -> bool {
    headers.contains_key(axum::http::header::UPGRADE)
}

fn should_forward_request_header(name: &str, is_upgrade: bool) -> bool {
    if name == "host" || name == "transfer-encoding" || name == "authorization" {
        return false;
    }

    if name == "connection" {
        return is_upgrade;
    }

    true
}

fn should_forward_response_header(name: &str, status: StatusCode) -> bool {
    if name == "transfer-encoding" {
        return false;
    }

    if name == "connection" {
        return status == StatusCode::SWITCHING_PROTOCOLS;
    }

    true
}

/// Configuration for relaying SSH agent connections back to the local machine.
#[derive(Clone)]
pub struct SshRelayConfig {
    /// Base URL of the git HTTP server (reachable via tunnel).
    pub url: String,
    /// Bearer token for authenticating to the git HTTP server.
    pub token: String,
}

/// Whether the startup setup task has completed.
type SetupDone = bool;

/// Shared state for the in-container HTTP server.
#[derive(Clone)]
pub struct PodServerState {
    pub pty_sessions: super::pty::PtySessions,
    pub ssh_relay: std::sync::Arc<tokio::sync::Mutex<Option<SshRelayConfig>>>,
    /// Repository path inside the container, set after the first /enter call.
    pub repo_path: std::sync::Arc<tokio::sync::Mutex<Option<PathBuf>>>,
    /// Pod name (the value also written to `git config rumpelpod.pod-name`).
    /// Used by /agent-files/<claude> to rebuild the statusLine on extract.
    pub pod_name: String,
    /// Codex app-server child process, started on first /codex connection.
    pub codex_app_server: super::codex::CodexAppServer,
    /// Current Claude Code session state, updated by hooks.
    pub claude_state: tokio::sync::watch::Sender<Option<super::types::ClaudeState>>,
    /// Current Codex session state, updated by the state monitor.
    pub codex_state: tokio::sync::watch::Sender<Option<super::types::CodexState>>,
    /// Whether the codex state monitor task has been spawned.
    pub codex_monitor_started: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Serializes recovery pushes triggered on /events connections.
    /// At launch the daemon's listener and a command's readiness probe
    /// can both connect and find the ref unpushed; without this they
    /// would race two `git push`es on the gateway, one losing the ref
    /// lock and logging an alarming (though harmless) error.
    pub push_gate: std::sync::Arc<tokio::sync::Semaphore>,
    /// Fully resolved environment: base container env + probed shell env +
    /// resolved remoteEnv.  Used for lifecycle commands, Claude, and Codex.
    pub resolved_env: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// If a foreground lifecycle command failed during startup, the
    /// error message is stored here.  The first /events connection
    /// takes and reports it; subsequent connections see None.
    pub lifecycle_error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Progress messages from the startup setup task (env probe, git,
    /// lifecycle).  The /events handler subscribes and forwards these
    /// as SSE `event: progress` before sending the `event: state`
    /// greeting.
    pub setup_progress: tokio::sync::broadcast::Sender<String>,
    /// Signals when the setup task has completed.
    pub setup_done: tokio::sync::watch::Receiver<SetupDone>,
    /// Gates test-only conveniences (the RUMPELPOD_SERVER_PORT env var
    /// export and the /write-home-files `${containerEnv:...}`
    /// substitution).  Not a public contract.
    pub test_mode: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorResponse {
    error: String,
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

pub fn run_container_server(
    token: String,
    repo_path: PathBuf,
    pod_name: String,
    local_env: Vec<String>,
    git_setup: Option<GitSetupParams>,
    test_mode: bool,
) -> ! {
    // Drop from root to the container user recorded at image build
    // time.  Sets uid, gid, supplementary groups, and $HOME/$USER.
    if let Err(e) = crate::switch_user::switch_user() {
        eprintln!("warning: failed to switch user: {e:#}");
    }

    // The daemon always starts tunnel-server before container-serve
    // so the git HTTP bridge is live when we clone the repo below;
    // read_required fails hard if that ordering ever breaks.
    let tunnel_port_path = Path::new(crate::port_file::TUNNEL_PORT_FILE);
    let tunnel_port =
        crate::port_file::read_required(tunnel_port_path).expect("reading tunnel port file");
    let tunnel_base_url = format!("http://127.0.0.1:{tunnel_port}");

    // -- Build server state and bind immediately --
    //
    // The /events endpoint is available right away so the daemon can
    // connect and receive setup progress.  The actual setup (env
    // probe, git, lifecycle) runs on a background task and streams
    // progress as SSE events.

    let ssh_relay_config = SshRelayConfig {
        url: tunnel_base_url.clone(),
        token: token.clone(),
    };

    let (setup_progress_tx, _) = tokio::sync::broadcast::channel::<String>(256);
    let (setup_done_tx, setup_done_rx) = tokio::sync::watch::channel::<SetupDone>(false);
    let lifecycle_error: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    let post_start_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resolved_env = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    let lifecycle_config = std::sync::Arc::new(std::sync::Mutex::new(
        crate::devcontainer::LifecycleConfig::default(),
    ));

    let (claude_state_tx, _) = tokio::sync::watch::channel(None);
    let (codex_state_tx, _) = tokio::sync::watch::channel(None);
    let state = PodServerState {
        pty_sessions: super::pty::PtySessions::new(),
        ssh_relay: std::sync::Arc::new(tokio::sync::Mutex::new(Some(ssh_relay_config))),
        repo_path: std::sync::Arc::new(tokio::sync::Mutex::new(Some(repo_path.clone()))),
        pod_name: pod_name.clone(),
        codex_app_server: super::codex::new_codex_app_server(),
        claude_state: claude_state_tx,
        codex_state: codex_state_tx,
        codex_monitor_started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        push_gate: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        resolved_env: resolved_env.clone(),
        lifecycle_error: lifecycle_error.clone(),
        setup_progress: setup_progress_tx.clone(),
        setup_done: setup_done_rx,
        test_mode,
    };

    // Unauthenticated LLM cache proxy: forwards API requests to the
    // git HTTP server on the local machine via the existing exec tunnel.
    // In test mode the daemon mounts the corresponding cache handler;
    // in production this is a harmless no-op (the daemon returns 404).
    let proxy_routes = Router::new()
        .route(
            "/llm-cache-proxy/{provider}/{*rest}",
            any(llm_cache_proxy_forward),
        )
        .with_state(state.clone());

    let app = Router::new().merge(proxy_routes);

    // Authenticated routes
    let authenticated_routes = Router::new()
        .route("/write-home-files", post(write_home_files_handler))
        .route("/fs/read", post(fs_read_handler))
        .route(
            "/git/patch",
            get(git_patch_get_handler).post(git_patch_post_handler),
        )
        .route("/git/push", post(git_push_handler))
        .route("/state", get(state_handler))
        .route(
            "/agent-files/{agent}",
            get(agent_files_get_handler).put(agent_files_put_handler),
        )
        .route("/cp", get(cp_download_handler).post(cp_upload_handler))
        .route("/init-mounts", post(init_mounts_handler))
        .route("/run", post(run_handler))
        .route("/events", get(events_handler))
        .route("/env", get(env_handler))
        .route("/container-env", get(container_env_handler))
        .route("/claude-state", post(claude_state_handler))
        .route("/codex-state", post(codex_state_handler))
        .route("/claude", any(super::pty::claude_session_handler))
        .route("/pi", any(super::pty::pi_session_handler))
        .route("/grok", any(super::pty::grok_session_handler))
        .route("/codex", any(super::codex::codex_ws_handler))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            token.clone(),
            require_bearer_token,
        ));

    let app = app
        .merge(authenticated_routes)
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(tower_http::decompression::RequestDecompressionLayer::new());

    block_on(async {
        // Start SSH relay listener.
        {
            let relay = state.ssh_relay.clone();
            let sock_path = Path::new(SSH_AGENT_SOCK_PATH);
            if let Some(parent) = sock_path.parent() {
                std::fs::create_dir_all(parent).expect("creating ssh-agent socket directory");
            }
            if sock_path.exists() {
                let _ = std::fs::remove_file(sock_path);
            }
            tokio::spawn(async move {
                if let Err(e) = run_ssh_agent_listener(relay).await {
                    eprintln!("ssh-agent relay listener failed: {e:#}");
                }
            });
        }

        // Reuse the previously recorded port if the file is still
        // around (stable ports across a container restart aid
        // debugging); otherwise let the kernel pick.  Remove the file
        // before binding so readers block until we write the
        // authoritative value below.
        let server_port_path = Path::new(crate::port_file::SERVER_PORT_FILE);
        let preferred =
            crate::port_file::read_preferred(server_port_path).expect("reading server port file");
        crate::port_file::remove_if_present(server_port_path)
            .expect("clearing stale server port file");
        let bind_addr = match preferred {
            Some(p) => format!("127.0.0.1:{p}"),
            None => "127.0.0.1:0".to_string(),
        };
        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .unwrap_or_else(|e| panic!("binding container server on {bind_addr}: {e}"));
        let port = listener
            .local_addr()
            .expect("getting container server local_addr")
            .port();

        // Persist token and port so container-exec, hook.rs, and the
        // daemon can reach us.
        std::fs::write(TOKEN_FILE, &token).expect("failed to write token file");
        crate::port_file::write_atomic(server_port_path, port).expect("writing server port file");

        // Tests point in-pod API clients at the LLM cache proxy via
        // `${containerEnv:RUMPELPOD_SERVER_PORT}` escapes; the value
        // is only known after we bind.  Gated on test_mode so
        // production devcontainers cannot accidentally rely on it.
        if test_mode {
            std::env::set_var("RUMPELPOD_SERVER_PORT", port.to_string());
        }

        eprintln!("container-serve listening on port {port}");

        // Run setup (env probe, git, lifecycle) on a background task.
        // Progress is streamed through setup_progress; the final
        // result goes into setup_done.  The /events handler subscribes
        // to both.
        //
        // SlowGuard messages go through an mpsc channel; forward them
        // into the broadcast progress channel.
        let (slow_tx, mut slow_rx) = tokio::sync::mpsc::channel::<String>(16);
        {
            let progress = setup_progress_tx.clone();
            tokio::spawn(async move {
                while let Some(msg) = slow_rx.recv().await {
                    let _ = progress.send(msg);
                }
            });
        }

        // If the setup task panics (e.g. git setup fails with
        // expect()), capture the message via catch_unwind and surface
        // it through lifecycle_error; the drop guard ensures
        // setup_done is signaled so /events does not hang.
        let lifecycle_error_for_panic = lifecycle_error.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = SetupDoneGuard { tx: &setup_done_tx };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_setup(
                    &repo_path,
                    &pod_name,
                    &local_env,
                    git_setup,
                    &tunnel_base_url,
                    &token,
                    &post_start_ran,
                    &resolved_env,
                    &lifecycle_config,
                    &lifecycle_error,
                    &setup_progress_tx,
                    &setup_done_tx,
                    &slow_tx,
                );
            }));
            if let Err(payload) = result {
                let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                    s.to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "<non-string panic payload>".to_string()
                };
                *lifecycle_error_for_panic.lock().unwrap() =
                    Some(format!("container setup panicked: {msg}"));
            }
        });

        axum::serve(listener, app).await.unwrap();
    });

    unreachable!("container server exited")
}

/// Signals setup_done on drop so /events never hangs.
struct SetupDoneGuard<'a> {
    tx: &'a tokio::sync::watch::Sender<SetupDone>,
}

impl Drop for SetupDoneGuard<'_> {
    fn drop(&mut self) {
        let _ = self.tx.send(true);
    }
}

/// Run the setup steps that used to block before the server accepted
/// connections.  Now runs on a background task after the server is
/// already listening.  Sends progress lines through `progress_tx` and
/// signals completion through `done_tx`.
#[allow(clippy::too_many_arguments)]
fn run_setup(
    repo_path: &Path,
    pod_name: &str,
    local_env: &[String],
    git_setup: Option<GitSetupParams>,
    tunnel_base_url: &str,
    token: &str,
    post_start_ran: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    resolved_env_out: &std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    lifecycle_config_out: &std::sync::Arc<std::sync::Mutex<crate::devcontainer::LifecycleConfig>>,
    lifecycle_error_out: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    progress_tx: &tokio::sync::broadcast::Sender<String>,
    done_tx: &tokio::sync::watch::Sender<SetupDone>,
    slow_tx: &tokio::sync::mpsc::Sender<String>,
) {
    // Best-effort progress reporting; receivers may not exist yet.
    let progress = |msg: &str| {
        let _ = progress_tx.send(msg.to_string());
        eprintln!("{msg}");
    };

    // -- Resolve environment --
    progress("resolving environment...");
    let (resolved_env, lifecycle_config) = resolve_pod_env(repo_path, pod_name, local_env);

    for (key, value) in &resolved_env {
        std::env::set_var(key, value);
    }
    *resolved_env_out.lock().unwrap() = resolved_env.clone();
    *lifecycle_config_out.lock().unwrap() = lifecycle_config.clone();

    // -- Git clone and setup (first launch only) --
    if let Some(ref params) = git_setup {
        let git_http_url = format!("{tunnel_base_url}/rumpelpod.git");

        let hook_path = repo_path.join(".git/hooks/reference-transaction");
        match std::fs::metadata(&hook_path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Clean baked checkouts can carry warm build caches whose mtimes
                // are part of cache validity.
                if git_setup::needs_sanitize_impl(repo_path).expect("sanitize check failed") {
                    progress("sanitizing repository...");
                    git_setup::sanitize_impl(repo_path).expect("sanitize failed");
                }
            }
            Err(e) => {
                let path = hook_path.display();
                panic!("checking hook {path}: {e}");
            }
        }

        progress("setting up git remotes...");
        {
            let _slow = crate::slow_guard::SlowGuard::new(
                "still fetching from host (git setup)...",
                slow_tx.clone(),
            );
            git_setup::setup_git_impl(&GitSetupRequest {
                repo_path: repo_path.to_path_buf(),
                url: git_http_url,
                token: token.to_string(),
                pod_name: pod_name.to_string(),
                extra_host_fetch: params.extra_host_fetch.clone(),
                branches: params.branches.clone(),
                primary: params.primary.clone(),
                git_identity: params.git_identity.clone(),
            })
            .expect("git setup failed");
        }

        progress("setting up submodules...");
        {
            let _slow = crate::slow_guard::SlowGuard::new(
                "still setting up submodules...",
                slow_tx.clone(),
            );
            git_setup::setup_submodules_impl(&GitSetupSubmodulesRequest {
                repo_path: repo_path.to_path_buf(),
                base_url: tunnel_base_url.to_string(),
                token: token.to_string(),
                pod_name: pod_name.to_string(),
                is_first_entry: true,
            })
            .expect("submodule setup failed");
        }
    }

    // -- Run lifecycle commands --
    progress("running lifecycle commands...");
    let _slow =
        crate::slow_guard::SlowGuard::new("still running lifecycle commands...", slow_tx.clone());
    let resp = super::lifecycle::run(&lifecycle_config, &resolved_env, post_start_ran, repo_path)
        .expect("lifecycle command execution failed");
    drop(_slow);
    for r in &resp.results {
        if r.exit_code != 0 {
            let stderr = super::types::base64_decode(&r.stderr)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let msg = format!("{} failed (exit {}): {}", r.name, r.exit_code, stderr);
            progress(&msg);
            *lifecycle_error_out.lock().unwrap() = Some(msg);
            break;
        }
    }

    progress("setup complete.");
    let _ = done_tx.send(true);
}

/// Middleware that rejects requests without a valid Bearer token.
async fn require_bearer_token(
    axum::extract::State(expected): axum::extract::State<String>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    let auth = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match auth {
        Some(value) if value == format!("Bearer {expected}") => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_json<T: Serialize>(val: T) -> Result<Json<T>, (StatusCode, Json<ErrorResponse>)> {
    Ok(Json(val))
}

fn err_json(e: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: format!("{e:#}"),
        }),
    )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// LLM cache proxy forwarding (test-only, unauthenticated)
// ---------------------------------------------------------------------------

/// Forward LLM API requests to the git HTTP server on the local
/// machine, which handles caching.  The ssh_relay config (set during
/// /enter) provides the tunnel URL and bearer token.
async fn llm_cache_proxy_forward(
    State(state): State<PodServerState>,
    axum::extract::Path((provider, rest)): axum::extract::Path<(String, String)>,
    req: axum::extract::Request,
) -> Response {
    let relay = state.ssh_relay.lock().await;
    let config = match relay.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "LLM cache proxy not ready (pod not entered yet)",
            )
                .into_response();
        }
    };
    drop(relay);

    let method = req.method().clone();

    // Preserve the query string if present.
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let target_url = format!("{}/llm-cache-proxy/{provider}/{rest}{query}", config.url);

    // Collect headers, replacing auth with the gateway bearer token.
    let is_upgrade = is_websocket_upgrade(req.headers());
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter(|(name, _)| should_forward_request_header(name.as_str(), is_upgrade))
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect();

    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("llm-cache-proxy forward: failed to read body: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let auth_header = format!("Bearer {}", config.token);
    tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("build llm-cache-proxy forwarding client");

        let mut request =
            client.request(method.as_str().parse().expect("parse method"), &target_url);
        request = request.header("authorization", &auth_header);
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request = request.body(body_bytes.to_vec());

        match request.send() {
            Ok(resp) => {
                let status = StatusCode::from_u16(resp.status().as_u16())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let mut builder = Response::builder().status(status);
                for (name, value) in resp.headers() {
                    let n = name.as_str();
                    if !should_forward_response_header(n, status) {
                        continue;
                    }
                    if let Ok(v) = value.to_str() {
                        builder = builder.header(n, v);
                    }
                }
                let body = resp.bytes().unwrap_or_default();
                builder.body(Body::from(body.to_vec())).unwrap()
            }
            Err(e) => {
                eprintln!("llm-cache-proxy forward: request failed: {e}");
                StatusCode::BAD_GATEWAY.into_response()
            }
        }
    })
    .await
    .expect("llm-cache-proxy forwarding task panicked")
}

// ---------------------------------------------------------------------------
// Events (SSE)
// ---------------------------------------------------------------------------

/// Format a single SSE event.
fn sse_event(event_type: &str, data: &str) -> String {
    format!("event: {event_type}\ndata: {data}\n\n")
}

/// Whether any local branch is ahead of, or missing from, its
/// rumpelpod remote-tracking ref -- i.e. the gateway has not seen the
/// pod's current branch state.
///
/// Successful pushes update `refs/remotes/rumpelpod/<branch>@<pod>`
/// and, for the configured primary branch, `refs/remotes/rumpelpod/<pod>`
/// (the `rumpelpod` remote's fetch refspec maps them).  A hook-push
/// that failed while the gateway was down leaves a remote-tracking ref
/// behind the branch, which is exactly the condition this detects.
/// Reads only local refs; no network call.
fn needs_push(repo_path: &Path, pod_name: &str) -> Result<bool> {
    let primary = primary_branch(repo_path)?;
    let output = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(objectname) %(refname:lstrip=2)",
            "refs/heads/",
        ])
        .current_dir(repo_path)
        .output()
        .context("listing local branches")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("git for-each-ref failed: {stderr}"));
    }
    let listing = String::from_utf8(output.stdout).context("branch listing was not UTF-8")?;
    for line in listing.lines() {
        let Some((sha, branch)) = line.split_once(' ') else {
            return Err(anyhow::anyhow!(
                "git for-each-ref returned malformed line: {line}"
            ));
        };
        let tracking = format!("refs/remotes/rumpelpod/{branch}@{pod_name}");
        if !ref_matches(repo_path, &tracking, sha)? {
            return Ok(true);
        }

        if branch == primary {
            let shortcut = format!("refs/remotes/rumpelpod/{pod_name}");
            if !ref_matches(repo_path, &shortcut, sha)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn primary_branch(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["config", "--get", "rumpelpod.pod-name"])
        .current_dir(repo_path)
        .output()
        .context("reading rumpelpod.pod-name")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "git config rumpelpod.pod-name failed: {stderr}"
        ));
    }
    let primary = String::from_utf8(output.stdout).context("primary branch was not UTF-8")?;
    let primary = primary.trim();
    if primary.is_empty() {
        return Err(anyhow::anyhow!("rumpelpod.pod-name is empty"));
    }
    Ok(primary.to_string())
}

fn ref_matches(repo_path: &Path, refname: &str, sha: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", refname])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("resolving ref {refname}"))?;
    match output.status.code() {
        Some(0) => {
            let tracked =
                String::from_utf8(output.stdout).context("tracked ref sha was not UTF-8")?;
            Ok(tracked.trim() == sha)
        }
        Some(1) => Ok(false),
        Some(_) | None => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(anyhow::anyhow!("git rev-parse {refname} failed: {stderr}"))
        }
    }
}

/// Force-push pod branches to the gateway, but only when local refs
/// show the gateway is behind.  Best-effort: failures are logged and
/// retried on the next /events connection or the next commit's
/// hook-push.
fn recover_push(repo_path: &Path, pod_name: &str) {
    match needs_push(repo_path, pod_name) {
        Ok(false) => return,
        Ok(true) => {}
        Err(e) => {
            eprintln!("events: deciding whether to push failed: {e:#}");
            return;
        }
    }
    match Command::new("git")
        .args(["push", "rumpelpod", "--force", "--quiet"])
        .current_dir(repo_path)
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1")
        .env("GIT_HTTP_LOW_SPEED_TIME", "10")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let status = output.status;
            eprintln!("events: git push rumpelpod exited {status}: {stderr}");
        }
        Err(e) => eprintln!("events: git push rumpelpod failed: {e}"),
        Ok(_) => {}
    }
}

/// SSE endpoint the daemon connects to while a pod is running.
///
/// On each new connection the pod recovers any branches the gateway
/// has not seen -- e.g. hook-pushes that failed while the gateway was
/// down.  Whether a push is owed is decided from local refs alone
/// (see `needs_push`) and the push itself runs on a background task,
/// so the `state` event is sent immediately, carrying the current
/// claude session state, followed by periodic keepalives and
/// claude_state change events.
async fn events_handler(State(state): State<PodServerState>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);

    // If setup is still in progress, stream progress events first.
    // Once setup completes, send the state greeting and continue
    // with keepalives and agent state changes.
    let mut setup_rx = state.setup_done.clone();
    let mut progress_rx = state.setup_progress.subscribe();

    let tx_setup = tx.clone();
    let state_for_task = state.clone();
    tokio::spawn(async move {
        // Stream setup progress until the setup task finishes.
        // If setup already completed before this connection, the
        // loop body never runs.
        loop {
            tokio::select! {
                biased;
                result = setup_rx.changed() => {
                    if result.is_err() {
                        break;
                    }
                    if *setup_rx.borrow() {
                        break;
                    }
                }
                msg = progress_rx.recv() => {
                    let Ok(msg) = msg else { break };
                    if tx_setup.send(sse_event("progress", &msg)).await.is_err() {
                        return;
                    }
                }
            }
        }

        // Take the lifecycle error so only the first /events caller
        // sees it.  Subsequent connections (re-entry) get None and
        // succeed, matching the old behavior.
        let lifecycle_error = state_for_task.lifecycle_error.lock().unwrap().take();

        // Recover any branches the gateway has not seen.  The decision
        // is made from local refs only and the push runs on a
        // background task, so the greeting below never waits on it.
        let repo_path = state_for_task.repo_path.lock().await.clone();
        if let Some(repo_path) = repo_path {
            let gate = state_for_task.push_gate.clone();
            let pod_name = state_for_task.pod_name.clone();
            tokio::spawn(async move {
                let Ok(permit) = gate.acquire_owned().await else {
                    return;
                };
                let result = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    recover_push(&repo_path, &pod_name);
                })
                .await;
                if let Err(e) = result {
                    eprintln!("events: recovery push task panicked: {e}");
                }
            });
        }

        // Send state greeting.
        let current_claude = *state_for_task.claude_state.borrow();
        let current_codex = *state_for_task.codex_state.borrow();
        let greeting_data = greeting_json(current_claude, current_codex, lifecycle_error);
        if tx_setup
            .send(sse_event("state", &greeting_data))
            .await
            .is_err()
        {
            return;
        }

        // Keepalives.
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if tx_setup.send(sse_event("keepalive", "{}")).await.is_err() {
                return;
            }
        }
    });

    // Forward claude state changes as SSE events.
    let mut claude_rx = state.claude_state.subscribe();
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while claude_rx.changed().await.is_ok() {
                let val = *claude_rx.borrow_and_update();
                let data =
                    serde_json::to_string(&val).expect("Option<ClaudeState> is serializable");
                if tx.send(sse_event("claude_state", &data)).await.is_err() {
                    return;
                }
            }
        });
    }

    // Forward codex state changes as SSE events.
    let mut codex_rx = state.codex_state.subscribe();
    tokio::spawn(async move {
        while codex_rx.changed().await.is_ok() {
            let val = *codex_rx.borrow_and_update();
            let data = serde_json::to_string(&val).expect("Option<CodexState> is serializable");
            if tx.send(sse_event("codex_state", &data)).await.is_err() {
                return;
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let stream = tokio_stream::StreamExt::map(stream, Ok::<_, std::convert::Infallible>);

    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .expect("building response never fails")
}

fn greeting_json(
    claude_state: Option<super::types::ClaudeState>,
    codex_state: Option<super::types::CodexState>,
    lifecycle_error: Option<String>,
) -> String {
    let cs = serde_json::to_value(claude_state).expect("ClaudeState is serializable");
    let xs = serde_json::to_value(codex_state).expect("CodexState is serializable");
    let mut obj = serde_json::json!({ "claude_state": cs, "codex_state": xs });
    if let Some(err) = lifecycle_error {
        obj["lifecycle_error"] = serde_json::Value::String(err);
    }
    serde_json::to_string(&obj).expect("greeting json is serializable")
}

/// Accept claude session state updates from hooks inside the container.
async fn claude_state_handler(
    State(state): State<PodServerState>,
    Json(req): Json<super::types::NotifyClaudeStateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state.claude_state.send_replace(Some(req.state));
    ok_json(serde_json::json!({}))
}

/// Accept codex session state updates.
///
/// The primary source of codex state is the WebSocket monitor
/// (pod::codex), but this endpoint allows state to be set directly
/// for testing or by future hook-based integrations.
async fn codex_state_handler(
    State(state): State<PodServerState>,
    Json(req): Json<super::types::NotifyCodexStateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state.codex_state.send_replace(Some(req.state));
    ok_json(serde_json::json!({}))
}

/// Look up the server's own user info (uid, home, shell).
fn get_user_info() -> Result<UserInfoResponse> {
    let uid = nix::unistd::getuid();
    let user = nix::unistd::User::from_uid(uid)
        .with_context(|| format!("looking up uid {uid}"))?
        .with_context(|| format!("uid {uid} not found in passwd"))?;
    Ok(UserInfoResponse {
        home: user.dir.to_string_lossy().to_string(),
        shell: user.shell.to_string_lossy().to_string(),
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
    })
}

// ---------------------------------------------------------------------------
// Write home files
// ---------------------------------------------------------------------------

async fn write_home_files_handler(
    State(state): State<PodServerState>,
    Json(req): Json<WriteHomeFilesRequest>,
) -> Result<Json<WriteHomeFilesResponse>, (StatusCode, Json<ErrorResponse>)> {
    let test_mode = state.test_mode;
    let result = tokio::task::spawn_blocking(move || write_home_files_impl(req, test_mode))
        .await
        .expect("write_home_files_impl panicked");
    match result {
        Ok(resp) => ok_json(resp),
        Err(e) => Err(err_json(e)),
    }
}

fn write_home_files_impl(
    req: WriteHomeFilesRequest,
    test_mode: bool,
) -> Result<WriteHomeFilesResponse> {
    let user_info = get_user_info()?;
    let home = Path::new(&user_info.home);

    // Write individual files
    for entry in &req.files {
        let dest = home.join(&entry.path);
        if entry.create_parents {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    let p = parent.display();
                    format!("creating parent dirs for {p}")
                })?;
            }
        }
        let mut content = base64_decode(&entry.content)?;
        // Substitute `${containerEnv:VAR}` placeholders so tests can
        // embed values only known post-bind (e.g.
        // `${containerEnv:RUMPELPOD_SERVER_PORT}` in a codex config.toml
        // shipped via write_home_files).  Skipped on binary payloads.
        if test_mode {
            if let Ok(text) = std::str::from_utf8(&content) {
                if text.contains("${containerEnv:") {
                    let substituted = crate::devcontainer::resolve_container_env_in_process(text);
                    content = substituted.into_bytes();
                }
            }
        }
        let dest_display = dest.display();
        std::fs::write(&dest, &content).with_context(|| format!("writing {dest_display}"))?;
    }

    // Extract tar archives
    for extract in &req.tar_extracts {
        let dest = home.join(&extract.dest);
        let dest_display = dest.display();
        std::fs::create_dir_all(&dest).with_context(|| format!("creating {dest_display}"))?;

        let data = base64_decode(&extract.data)?;
        let mut archive = tar::Archive::new(data.as_slice());
        for entry in archive
            .entries()
            .with_context(|| format!("reading tar entries for {dest_display}"))?
        {
            let mut entry =
                entry.with_context(|| format!("reading tar entry for {dest_display}"))?;
            // Strip the leading "./" that tar -c . produces
            let path = entry.path()?.into_owned();
            let path = path.strip_prefix(".").unwrap_or(&path);
            if path.as_os_str().is_empty() {
                continue;
            }
            let target = dest.join(path);
            if entry.header().entry_type().is_dir() {
                std::fs::create_dir_all(&target)?;
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                entry.unpack(&target).with_context(|| {
                    let t = target.display();
                    format!("extracting to {t}")
                })?;
            }
        }
    }

    Ok(WriteHomeFilesResponse {
        home: user_info.home,
    })
}

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

async fn fs_read_handler(
    Json(req): Json<FsReadRequest>,
) -> Result<Json<FsReadResponse>, (StatusCode, Json<ErrorResponse>)> {
    let path = req.path.display();
    let data =
        std::fs::read(&req.path).map_err(|e| err_json(anyhow::anyhow!("reading {path}: {e}")))?;
    ok_json(FsReadResponse {
        content: base64_encode(&data),
    })
}

// Dead copies of setup_git_impl, install_hook_impl, SubmoduleEntry,
// detect_submodules_from_gitmodules, setup_submodules_impl, configure_submodule,
// and sanitize_impl were removed -- they now live in git_setup.rs.

/// GET /git/patch -- return the dirty working tree as a binary patch.
///
/// Empty body means a clean tree.  The pod server knows its own
/// repo_path; callers (daemon, fork, recreate) need not pass it in.
async fn git_patch_get_handler(
    State(state): State<PodServerState>,
) -> Result<axum::response::Response, (StatusCode, Json<ErrorResponse>)> {
    let repo_path = state
        .repo_path
        .lock()
        .await
        .clone()
        .ok_or_else(|| err_json(anyhow::anyhow!("repo_path not set yet")))?;

    let patch = tokio::task::spawn_blocking(move || snapshot_impl(&repo_path))
        .await
        .expect("snapshot_impl panicked")
        .map_err(err_json)?;

    let body = patch.unwrap_or_default();
    Ok(axum::response::Response::builder()
        .header("Content-Type", "application/octet-stream")
        .body(axum::body::Body::from(body))
        .unwrap())
}

fn snapshot_impl(repo_path: &Path) -> Result<Option<Vec<u8>>> {
    // `git add -A` followed by a cached diff so the patch covers
    // untracked files too.  Plain `git diff` would miss them, which
    // is not what "dirty working tree" means for the fork/recreate
    // callers.
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo_path)
        .success()?;

    let diff = Command::new("git")
        .args(["diff", "--binary", "--cached"])
        .current_dir(repo_path)
        .success()
        .context("git diff")?;

    if diff.is_empty() {
        Ok(None)
    } else {
        Ok(Some(diff))
    }
}

/// POST /git/patch -- apply a binary patch produced by GET /git/patch.
///
/// Body is the raw patch bytes (no base64, no JSON wrapper).  The
/// server parses created-file paths out of the patch itself so callers
/// only need to forward the body verbatim.
async fn git_patch_post_handler(
    State(state): State<PodServerState>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let repo_path = state
        .repo_path
        .lock()
        .await
        .clone()
        .ok_or_else(|| err_json(anyhow::anyhow!("repo_path not set yet")))?;

    tokio::task::spawn_blocking(move || apply_patch_impl(&repo_path, &body))
        .await
        .expect("apply_patch_impl panicked")
        .map_err(err_json)?;

    ok_json(serde_json::json!({}))
}

fn apply_patch_impl(repo_path: &Path, patch: &[u8]) -> Result<()> {
    // Files created by the patch may already exist from the image; remove
    // them first so `git apply` does not bail.  Best-effort: missing
    // entries are normal for a fresh tree.
    let created_files = parse_created_files(patch).unwrap_or_default();
    for file in &created_files {
        let _ = std::fs::remove_file(repo_path.join(file));
    }

    let mut child = Command::new("git")
        .args(["apply", "-"])
        .current_dir(repo_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git apply")?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(patch)
            .context("writing patch to git apply")?;
    }

    let output = child.wait_with_output().context("waiting for git apply")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("git apply failed: {stderr}"));
    }

    let _ = Command::new("git")
        .args(["submodule", "update", "--recursive"])
        .current_dir(repo_path)
        .status();

    Ok(())
}

/// GET /state -- pure read of pod metadata used by `rumpel fork`.
///
/// Reports every local branch (with sha + upstream as a short string),
/// the primary branch (`git config rumpelpod.pod-name`), per-agent
/// state existence, and whether the working tree is dirty.
async fn state_handler(
    State(state): State<PodServerState>,
) -> Result<Json<StateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let repo_path = state
        .repo_path
        .lock()
        .await
        .clone()
        .ok_or_else(|| err_json(anyhow::anyhow!("repo_path not set yet")))?;

    tokio::task::spawn_blocking(move || build_state_response(&repo_path))
        .await
        .expect("state_handler panicked")
        .map(Json)
        .map_err(err_json)
}

fn build_state_response(repo_path: &Path) -> Result<StateResponse> {
    let branches_out = Command::new("git")
        .args([
            "for-each-ref",
            "refs/heads/",
            "--format=%(refname:short)%00%(objectname)%00%(upstream:short)",
        ])
        .current_dir(repo_path)
        .success()
        .context("listing local branches")?;
    let branches_text = String::from_utf8(branches_out).context("for-each-ref output utf8")?;
    let mut branches = Vec::new();
    for line in branches_text.lines() {
        let mut parts = line.split('\0');
        let name = parts.next().unwrap_or("").to_string();
        let sha = parts.next().unwrap_or("").to_string();
        let upstream_raw = parts.next().unwrap_or("").to_string();
        if name.is_empty() || sha.is_empty() {
            continue;
        }
        let upstream = if upstream_raw.is_empty() {
            None
        } else {
            Some(upstream_raw)
        };
        branches.push(BranchInfo {
            name,
            sha,
            upstream,
        });
    }

    let primary_out = Command::new("git")
        .args(["config", "rumpelpod.pod-name"])
        .current_dir(repo_path)
        .success()
        .context("reading rumpelpod.pod-name")?;
    let primary = String::from_utf8(primary_out)
        .context("rumpelpod.pod-name utf8")?
        .trim()
        .to_string();

    let dirty_out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .success()
        .context("git status --porcelain")?;
    let dirty = !dirty_out.is_empty();

    let home = nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.dir)
        .unwrap_or_else(|| PathBuf::from("/root"));
    let has_claude_state = home.join(".claude").exists() || home.join(".claude.json").exists();
    let has_codex_state = home.join(".codex").exists();
    let has_pi_state = home.join(".pi").exists();
    let has_grok_state = home.join(".grok").exists();

    Ok(StateResponse {
        branches,
        primary,
        has_claude_state,
        has_codex_state,
        has_pi_state,
        has_grok_state,
        dirty,
    })
}

/// POST /git/push -- push every local branch to the rumpelpod remote.
///
/// Forks call this on the source pod before they fetch from the host:
/// pod-only branches (never pushed since the last events reconnect)
/// only land on the host after this push, and the new pod's
/// `extra_host_fetch` needs them present.
async fn git_push_handler(
    State(state): State<PodServerState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let repo_path = state
        .repo_path
        .lock()
        .await
        .clone()
        .ok_or_else(|| err_json(anyhow::anyhow!("repo_path not set yet")))?;

    tokio::task::spawn_blocking(move || {
        let output = Command::new("git")
            .args(["push", "rumpelpod", "--force", "--quiet"])
            .current_dir(&repo_path)
            .env("GIT_HTTP_LOW_SPEED_LIMIT", "1")
            .env("GIT_HTTP_LOW_SPEED_TIME", "10")
            .output()
            .context("spawning git push")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("git push rumpelpod failed: {stderr}"));
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .expect("git push task panicked")
    .map_err(err_json)?;

    ok_json(serde_json::json!({}))
}

/// Parse `git apply --summary` output to find files the patch creates.
fn parse_created_files(patch: &[u8]) -> Result<Vec<String>> {
    use std::io::Write;

    let mut child = Command::new("git")
        .args(["apply", "--summary", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning git apply --summary")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch).context("writing patch to summary")?;
    }

    let output = child
        .wait_with_output()
        .context("waiting for git apply --summary")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut created = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("create mode ") {
            // e.g. "create mode 100644 path/to/file"
            if let Some(idx) = rest.find(' ') {
                created.push(rest[idx + 1..].to_string());
            }
        }
    }
    Ok(created)
}

fn probe_env_impl(shell_flags: &str) -> Result<HashMap<String, String>> {
    // Check if bash is available
    let has_bash = Command::new("which")
        .arg("bash")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s: std::process::ExitStatus| s.success());

    if !has_bash {
        return Ok(HashMap::new());
    }

    // Get base environment
    let base_output = Command::new("env")
        .arg("-0")
        .output()
        .context("getting base env")?;
    let base = parse_null_delimited_env(&base_output.stdout);

    // Get probed environment via bash with shell flags.
    //
    // Use tokio's Command with a timeout.  std Command::output() waits
    // for pipe EOF, which hangs if a shell profile spawns a background
    // process that inherits stdout.  Tokio reads the pipes concurrently
    // (no 64KB deadlock) and the timeout kills the wait if a grandchild
    // lingers.
    let output = crate::async_runtime::RUNTIME.block_on(async {
        let child = tokio::process::Command::new("bash")
            .arg(shell_flags)
            .arg("env -0")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning bash for env probe")?;

        tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
            .await
            .context("env probe timed out")?
            .context("env probe failed")
    })?;
    if !output.status.success() {
        let status = output.status;
        return Err(anyhow::anyhow!("env probe exited with {status}"));
    }

    let probed = parse_null_delimited_env(&output.stdout);

    let skip = ["_", "SHLVL", "BASH_EXECUTION_STRING"];
    Ok(probed
        .into_iter()
        .filter(|(key, value)| !skip.contains(&key.as_str()) && base.get(key) != Some(value))
        .collect())
}

fn parse_null_delimited_env(data: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(data);
    let mut map = HashMap::new();
    for entry in text.split('\0') {
        if let Some((key, value)) = entry.split_once('=') {
            if !key.is_empty() {
                map.insert(key.to_string(), value.to_string());
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Pod environment resolution
// ---------------------------------------------------------------------------

/// Baked devcontainer.json written by the prepared image build.
const DEVCONTAINER_CONFIG_PATH: &str = "/opt/rumpelpod/devcontainer.json";

/// Resolve the full environment for the pod on startup.
///
/// Reads the baked devcontainer.json, resolves `${localEnv:...}` from the
/// CLI flags and `${containerEnv:...}` from the process environment,
/// probes the user's shell, and merges everything into a single env map.
fn resolve_pod_env(
    _repo_path: &Path,
    _pod_name: &str,
    local_env_args: &[String],
) -> (
    HashMap<String, String>,
    crate::devcontainer::LifecycleConfig,
) {
    use crate::devcontainer::{
        resolve_container_env_in_process, resolve_local_env_from_map, DevContainer, UserEnvProbe,
    };

    // Parse --local-env KEY=VALUE args into a map.
    let local_env: HashMap<String, String> = local_env_args
        .iter()
        .filter_map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();

    // Read the baked devcontainer.json.  This is the literal file from
    // the host repo, baked into the prepared image by the daemon.
    let dc: DevContainer = match std::fs::read_to_string(DEVCONTAINER_CONFIG_PATH) {
        Ok(json) => json5::from_str(&json).unwrap_or_else(|e| {
            eprintln!("warning: failed to parse {DEVCONTAINER_CONFIG_PATH}: {e:#}");
            DevContainer::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DevContainer::default(),
        Err(e) => {
            eprintln!("warning: failed to read {DEVCONTAINER_CONFIG_PATH}: {e:#}");
            DevContainer::default()
        }
    };

    let lifecycle_config = dc.lifecycle.clone();

    // Resolve ${localEnv:...} and ${containerEnv:...} in remoteEnv values,
    // and build the final merged environment.
    let remote_env = dc.remote_env.unwrap_or_default();
    let mut resolved_remote: HashMap<String, String> = HashMap::new();
    for (key, value) in &remote_env {
        let after_local = resolve_local_env_from_map(value, &local_env);
        let after_container = resolve_container_env_in_process(&after_local);
        resolved_remote.insert(key.clone(), after_container);
    }

    // Start with the current process environment (image ENV + containerEnv).
    let mut env: HashMap<String, String> = std::env::vars().collect();

    // Overlay probed shell env additions.
    let probe = dc
        .user_env_probe
        .as_ref()
        .unwrap_or(&UserEnvProbe::LoginInteractiveShell);
    if let Some(flags) = probe.shell_flags_exec() {
        let probed = probe_env_impl(flags).unwrap_or_else(|e| {
            eprintln!("userEnvProbe failed: {e:#}");
            HashMap::new()
        });
        for (key, value) in probed {
            env.insert(key, value);
        }
    }

    // Overlay resolved remoteEnv (highest priority).
    for (key, value) in resolved_remote {
        env.insert(key, value);
    }

    (env, lifecycle_config)
}

/// Return the resolved environment as JSON.
async fn env_handler(State(state): State<PodServerState>) -> Json<HashMap<String, String>> {
    Json(state.resolved_env.lock().unwrap().clone())
}

/// Return only the env vars the daemon set as `containerEnv` at launch
/// time, with their current values from the pod process environment.
///
/// The set of keys is read from `/opt/rumpelpod/container-env-keys`,
/// baked into the prepared image by the daemon.  Values come from
/// `std::env::var` so any in-pod mutations are reflected.  Keys that
/// have been unset in-process are omitted.
///
/// Used by `rumpel fork` to snapshot the source pod's user-configured
/// env without leaking incidental vars (PATH, HOME, SSH_AUTH_SOCK,
/// shell-probe additions, ...).
async fn container_env_handler() -> Json<HashMap<String, String>> {
    let path = crate::prepared_image::CONTAINER_ENV_KEYS_PATH;
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            eprintln!("warning: reading {path}: {e:#}");
            return Json(HashMap::new());
        }
    };
    let mut out = HashMap::new();
    for key in contents.lines() {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            out.insert(key.to_string(), value);
        }
    }
    Json(out)
}

// ---------------------------------------------------------------------------
// Streaming helpers for tar transfer
// ---------------------------------------------------------------------------

/// Adapter: sends written bytes as chunks through a tokio mpsc channel.
/// Used to stream tar output into an HTTP response body.
struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let bytes = axum::body::Bytes::copy_from_slice(buf);
        self.tx
            .blocking_send(Ok(bytes))
            .map_err(|_| std::io::Error::other("receiver dropped"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Copy handlers
// ---------------------------------------------------------------------------

async fn cp_download_handler(
    axum::extract::Query(req): axum::extract::Query<CpDownloadRequest>,
) -> Result<axum::response::Response, (StatusCode, Json<ErrorResponse>)> {
    // Validate the path before starting the stream, so we can return
    // a proper error response for the most common failure (not found).
    let check_path = req.path.clone();
    let meta = tokio::task::spawn_blocking(move || std::fs::symlink_metadata(&check_path))
        .await
        .expect("stat panicked");
    if let Err(e) = meta {
        let path_display = req.path.display();
        return Err(err_json(anyhow::anyhow!("stat {path_display}: {e}")));
    }

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::task::spawn_blocking(move || {
        let writer = ChannelWriter { tx };
        if let Err(e) = cp_download_impl(req, writer) {
            eprintln!("cp_download error: {e:#}");
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(axum::response::Response::builder()
        .header("Content-Type", "application/x-tar")
        .body(axum::body::Body::from_stream(stream))
        .unwrap())
}

fn cp_download_impl(req: CpDownloadRequest, writer: impl std::io::Write) -> Result<()> {
    let path = &req.path;
    let path_display = path.display();

    let mut archive = tar::Builder::new(writer);
    archive.follow_symlinks(req.follow_symlinks);

    let meta = std::fs::symlink_metadata(path).with_context(|| format!("stat {path_display}"))?;

    let name = path
        .file_name()
        .with_context(|| format!("no file name in {path_display}"))?;
    let wrapper_name = Path::new("_").join(name);

    if meta.is_dir() {
        archive
            .append_dir_all(&wrapper_name, path)
            .with_context(|| format!("archiving directory {path_display}"))?;
    } else {
        archive
            .append_path_with_name(path, &wrapper_name)
            .with_context(|| format!("archiving file {path_display}"))?;
    }

    archive
        .into_inner()
        .with_context(|| format!("finalizing tar for {path_display}"))?;
    Ok(())
}

async fn cp_upload_handler(
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let path = headers
        .get("X-Path")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| err_json(anyhow::anyhow!("missing X-Path header")))?;
    let path = PathBuf::from(path);

    use tokio_stream::StreamExt;
    let stream = body
        .into_data_stream()
        .map(|result| result.map_err(std::io::Error::other));
    let async_reader = tokio_util::io::StreamReader::new(stream);
    let sync_reader = tokio_util::io::SyncIoBridge::new(async_reader);

    let result = tokio::task::spawn_blocking(move || cp_upload_impl(&path, sync_reader))
        .await
        .expect("cp_upload_impl panicked");
    result.map_err(err_json)?;
    ok_json(serde_json::json!({}))
}

fn cp_upload_impl(path: &Path, reader: impl std::io::Read) -> Result<()> {
    let path_display = path.display();
    // Match standard cp: when the destination is an existing directory,
    // place the content inside it rather than overwriting it.
    let dest_is_dir = path.is_dir();

    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .with_context(|| format!("reading tar entries for {path_display}"))?
    {
        let mut entry = entry.with_context(|| format!("reading tar entry for {path_display}"))?;
        let entry_path = entry.path().context("reading entry path")?.into_owned();

        let relative = match entry_path.strip_prefix("_") {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };

        let target = if dest_is_dir {
            path.join(relative)
        } else {
            let mut components = relative.components();
            components.next();
            let rest: PathBuf = components.collect();

            if rest.as_os_str().is_empty() {
                path.to_path_buf()
            } else {
                path.join(&rest)
            }
        };

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).with_context(|| format!("creating {path_display}"))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating parent for {path_display}"))?;
            }
            let target_display = target.display();
            entry
                .unpack(&target)
                .with_context(|| format!("extracting to {target_display}"))?;
        }
    }

    Ok(())
}

/// Populate bind mount targets from a single tar archive.
///
/// Entries use absolute destination paths (leading slash stripped in the
/// tar, restored during extraction).  The daemon builds one tar covering
/// all bind mounts so this is a single-request operation.
async fn init_mounts_handler(
    body: axum::body::Body,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    use tokio_stream::StreamExt;
    let stream = body
        .into_data_stream()
        .map(|result| result.map_err(std::io::Error::other));
    let async_reader = tokio_util::io::StreamReader::new(stream);
    let sync_reader = tokio_util::io::SyncIoBridge::new(async_reader);

    let result = tokio::task::spawn_blocking(move || init_mounts_impl(sync_reader))
        .await
        .expect("init_mounts_impl panicked");
    result.map_err(err_json)?;
    ok_json(serde_json::json!({}))
}

fn init_mounts_impl(reader: impl std::io::Read) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let entry_path = entry.path().context("reading entry path")?.into_owned();

        // Entries are stored without a leading slash; restore it to get the
        // absolute container path.
        let target = Path::new("/").join(&entry_path);

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).with_context(|| {
                let target = target.display();
                format!("creating directory {target}")
            })?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    let parent = parent.display();
                    format!("creating parent directory {parent}")
                })?;
            }
            entry.unpack(&target).with_context(|| {
                let target = target.display();
                format!("extracting to {target}")
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(repo_path: &Path, args: &[&str]) {
        Command::new("git")
            .args(args)
            .current_dir(repo_path)
            .success()
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e:#}"));
    }

    fn git_stdout(repo_path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo_path)
            .success()
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e:#}"));
        String::from_utf8(output).unwrap().trim().to_string()
    }

    #[test]
    fn needs_push_detects_stale_primary_shortcut() {
        let temp_dir = TempDir::with_prefix("rumpelpod-needs-push-").unwrap();
        let repo_path = temp_dir.path();

        git(repo_path, &["init", "--initial-branch=pod"]);
        git(repo_path, &["config", "user.email", "test@example.com"]);
        git(repo_path, &["config", "user.name", "Rumpelpod Test"]);
        git(repo_path, &["config", "rumpelpod.pod-name", "pod"]);

        std::fs::write(repo_path.join("file.txt"), "one\n").unwrap();
        git(repo_path, &["add", "file.txt"]);
        git(repo_path, &["commit", "-m", "initial"]);
        let initial = git_stdout(repo_path, &["rev-parse", "HEAD"]);
        git(
            repo_path,
            &["update-ref", "refs/remotes/rumpelpod/pod@pod", &initial],
        );
        assert!(needs_push(repo_path, "pod").unwrap());

        git(
            repo_path,
            &["update-ref", "refs/remotes/rumpelpod/pod", &initial],
        );
        assert!(!needs_push(repo_path, "pod").unwrap());

        std::fs::write(repo_path.join("file.txt"), "two\n").unwrap();
        git(repo_path, &["commit", "-am", "second"]);
        let second = git_stdout(repo_path, &["rev-parse", "HEAD"]);
        git(
            repo_path,
            &["update-ref", "refs/remotes/rumpelpod/pod@pod", &second],
        );

        assert!(needs_push(repo_path, "pod").unwrap());

        git(
            repo_path,
            &["update-ref", "refs/remotes/rumpelpod/pod", &second],
        );
        assert!(!needs_push(repo_path, "pod").unwrap());
    }
}

// ---------------------------------------------------------------------------
// Agent files (per-agent state transfer for fork and recreate)
// ---------------------------------------------------------------------------

/// Per-agent registry: paths under $HOME that constitute the agent's state.
/// Order is preserved in the tar.
fn agent_paths(agent: &str) -> Option<&'static [&'static str]> {
    match agent {
        "claude" => Some(&[".claude.json", ".claude"]),
        "codex" => Some(&[".codex"]),
        "pi" => Some(&[".pi"]),
        "grok" => Some(&[".grok"]),
        _ => None,
    }
}

/// Resolve the home directory of the user this server runs as.
fn server_home() -> Result<PathBuf> {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .context("looking up server uid")?
        .context("server uid not found in passwd")?;
    Ok(user.dir)
}

/// GET /agent-files/<agent> -- stream a tar.gz of the agent's paths.
///
/// Returns 404 if none of the registered paths exist.  Built with
/// `tar::Builder` on a pipe-thread and gzip-compressed, mirroring
/// `cp::tar_local_path` so memory stays bounded for large home dirs.
async fn agent_files_get_handler(
    State(state): State<PodServerState>,
    axum::extract::Path(agent): axum::extract::Path<String>,
) -> Result<axum::response::Response, (StatusCode, Json<ErrorResponse>)> {
    let _ = state; // PodServerState is the auth gate; nothing else needed.
    let paths = agent_paths(&agent).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("unknown agent: {agent}"),
            }),
        )
    })?;

    let home = server_home().map_err(err_json)?;

    // Filter to entries that exist; if none do, the agent has no
    // state to transfer.
    let existing: Vec<&'static str> = paths
        .iter()
        .copied()
        .filter(|p| home.join(p).exists())
        .collect();
    if existing.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("no state for agent {agent}"),
            }),
        ));
    }

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let home_for_thread = home.clone();
    tokio::task::spawn_blocking(move || {
        let writer = ChannelWriter { tx };
        let mut archive = tar::Builder::new(writer);
        archive.follow_symlinks(false);
        for rel in &existing {
            let full = home_for_thread.join(rel);
            let res = if full.is_dir() {
                archive.append_dir_all(rel, &full)
            } else {
                archive.append_path_with_name(&full, rel)
            };
            if let Err(e) = res {
                eprintln!("agent-files {rel}: {e:#}");
                return;
            }
        }
        if let Err(e) = archive.into_inner() {
            eprintln!("agent-files finalize: {e:#}");
        }
    });

    // The router's CompressionLayer compresses on Accept-Encoding;
    // we just emit raw tar.  Otherwise reqwest's auto-decode (built
    // with .gzip(true)) and our manual GzEncoder would double-decode.
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(axum::response::Response::builder()
        .header("Content-Type", "application/x-tar")
        .body(axum::body::Body::from_stream(stream))
        .unwrap())
}

#[derive(Debug, Deserialize)]
struct AgentFilesPutQuery {
    /// Override for the PermissionRequest hook on claude.  When omitted,
    /// whatever the uploaded settings.json carries is preserved.
    permission_hook: Option<bool>,
}

/// PUT /agent-files/<agent> -- extract a tar.gz body under $HOME, then
/// run any per-agent post-extraction rewrites.
///
/// For `claude`, the rewrites are: overwrite `~/.claude/settings.json`
/// `statusLine` with this pod's canonical command, and overwrite the
/// notify-state hooks with the rumpelpod canonical entries.  The
/// PermissionRequest hook is left as-is unless `?permission_hook=` is
/// set.
///
/// For `codex` and `grok`, no post-extraction work.
async fn agent_files_put_handler(
    State(state): State<PodServerState>,
    axum::extract::Path(agent): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<AgentFilesPutQuery>,
    body: axum::body::Body,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    if agent_paths(&agent).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("unknown agent: {agent}"),
            }),
        ));
    }

    use tokio_stream::StreamExt;
    let stream = body
        .into_data_stream()
        .map(|result| result.map_err(std::io::Error::other));
    let async_reader = tokio_util::io::StreamReader::new(stream);
    let sync_reader = tokio_util::io::SyncIoBridge::new(async_reader);

    let pod_name = state.pod_name.clone();
    let test_mode = state.test_mode;
    let agent_for_blocking = agent.clone();
    tokio::task::spawn_blocking(move || {
        agent_files_put_impl(
            &agent_for_blocking,
            sync_reader,
            &pod_name,
            query.permission_hook,
            test_mode,
        )
    })
    .await
    .expect("agent_files_put_impl panicked")
    .map_err(err_json)?;

    ok_json(serde_json::json!({}))
}

fn agent_files_put_impl(
    agent: &str,
    reader: impl std::io::Read,
    pod_name: &str,
    permission_hook: Option<bool>,
    test_mode: bool,
) -> Result<()> {
    let home = server_home()?;

    // The router's RequestDecompressionLayer already gunzipped the
    // body when the client sent Content-Encoding: gzip, so the reader
    // here yields plain tar bytes.
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(false);
    archive.set_overwrite(true);
    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let rel = entry.path().context("reading entry path")?.into_owned();
        if rel.as_os_str().is_empty() || rel.is_absolute() {
            continue;
        }
        let target = home.join(&rel);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).with_context(|| {
                let t = target.display();
                format!("creating {t}")
            })?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    let p = parent.display();
                    format!("creating parent {p}")
                })?;
            }
            entry.unpack(&target).with_context(|| {
                let t = target.display();
                format!("extracting to {t}")
            })?;
            // Test-mode mirror of write_home_files: substitute
            // `${containerEnv:VAR}` placeholders so codex tests can
            // bake the post-bind RUMPELPOD_SERVER_PORT into config.toml.
            // Skipped on binary payloads.
            if test_mode {
                if let Ok(text) = std::fs::read_to_string(&target) {
                    if text.contains("${containerEnv:") {
                        let substituted =
                            crate::devcontainer::resolve_container_env_in_process(&text);
                        std::fs::write(&target, substituted).with_context(|| {
                            let t = target.display();
                            format!("rewriting containerEnv placeholders in {t}")
                        })?;
                    }
                }
            }
        }
    }

    match agent {
        "claude" => rewrite_claude_settings(&home, pod_name, permission_hook)?,
        "codex" => {}
        "pi" => {}
        // Grok needs no post-extraction work: its notify-state hooks are
        // not wired up, so there is no settings file to rewrite.
        "grok" => {}
        // agent_paths() already validated the name; this is unreachable.
        _ => return Err(anyhow::anyhow!("unknown agent: {agent}")),
    }

    Ok(())
}

/// Read ~/.claude/settings.json (or {} if absent), force the rumpelpod
/// statusLine + notify-state hooks, optionally toggle PermissionRequest,
/// and write the result back.
fn rewrite_claude_settings(
    home: &Path,
    pod_name: &str,
    permission_hook_override: Option<bool>,
) -> Result<()> {
    let settings_path = home.join(".claude/settings.json");
    let raw = match std::fs::read(&settings_path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => b"{}".to_vec(),
        Err(e) => return Err(anyhow::Error::from(e).context("reading settings.json")),
    };
    let mut obj: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&raw).unwrap_or_default();

    let escaped = pod_name.replace('\'', "'\\''");
    let cmd = format!("echo 'Rumpelpod: {escaped} | Ctrl-a d to detach'");
    obj.insert(
        "statusLine".to_string(),
        serde_json::json!({"type": "command", "command": cmd}),
    );

    // Existing hooks subobject (preserved across extraction); rewrite
    // only the rumpelpod-controlled keys, leave everything else alone.
    let mut hooks = obj
        .get("hooks")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let bin = "/opt/rumpelpod/bin/rumpel";
    hooks.insert(
        "UserPromptSubmit".to_string(),
        serde_json::json!([{
            "matcher": "",
            "hooks": [{ "type": "command", "command":
                format!("{bin} claude-hook notify-state processing") }]
        }]),
    );
    hooks.insert(
        "Stop".to_string(),
        serde_json::json!([{
            "matcher": "",
            "hooks": [{ "type": "command", "command":
                format!("{bin} claude-hook notify-state waiting_for_input") }]
        }]),
    );
    hooks.insert(
        "StopFailure".to_string(),
        serde_json::json!([{
            "matcher": "authentication_failed",
            "hooks": [{ "type": "command", "command":
                format!("{bin} claude-hook notify-state auth_error") }]
        }]),
    );
    hooks.insert(
        "SessionEnd".to_string(),
        serde_json::json!([{
            "matcher": "",
            "hooks": [{ "type": "command", "command":
                format!("{bin} claude-hook notify-state stopped") }]
        }]),
    );

    if let Some(want) = permission_hook_override {
        if want {
            hooks.insert(
                "PermissionRequest".to_string(),
                serde_json::json!([{
                    "matcher": "",
                    "hooks": [{ "type": "command", "command":
                        format!("{bin} claude-hook permission-request") }]
                }]),
            );
        } else {
            hooks.remove("PermissionRequest");
        }
    }

    obj.insert("hooks".to_string(), serde_json::Value::Object(hooks));

    let pretty = serde_json::to_vec_pretty(&obj).context("serializing settings.json")?;
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            let p = parent.display();
            format!("creating {p}")
        })?;
    }
    let display = settings_path.display();
    std::fs::write(&settings_path, &pretty).with_context(|| format!("writing {display}"))?;
    Ok(())
}

async fn run_handler(
    Json(req): Json<RunRequest>,
) -> Result<Json<RunResponse>, (StatusCode, Json<ErrorResponse>)> {
    let result = tokio::task::spawn_blocking(|| run_impl(req))
        .await
        .expect("run_impl panicked");
    match result {
        Ok(resp) => ok_json(resp),
        Err(e) => Err(err_json(e)),
    }
}

fn run_impl(req: RunRequest) -> Result<RunResponse> {
    use std::io::Read;

    if req.cmd.is_empty() {
        return Err(anyhow::anyhow!("empty command"));
    }

    let mut cmd = Command::new(&req.cmd[0]);
    cmd.args(&req.cmd[1..]);

    if let Some(ref workdir) = req.workdir {
        cmd.current_dir(workdir);
    }

    for env_str in &req.env {
        if let Some((key, value)) = env_str.split_once('=') {
            cmd.env(key, value);
        }
    }

    let has_stdin = req.stdin.is_some();
    if has_stdin {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning command")?;

    if let Some(ref stdin_b64) = req.stdin {
        let data = base64_decode(stdin_b64)?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(&data).context("writing stdin")?;
        }
    }

    // Drain stdout and stderr in background threads to prevent deadlock
    // when the child produces more output than the OS pipe buffer (~64KB).
    let child_stdout = child.stdout.take().expect("stdout was piped");
    let child_stderr = child.stderr.take().expect("stderr was piped");

    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = child_stdout;
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = child_stderr;
        let _ = reader.read_to_end(&mut buf);
        buf
    });

    let timeout = req.timeout_secs.map(std::time::Duration::from_secs);

    let (status, timed_out) = if let Some(dur) = timeout {
        let start = std::time::Instant::now();
        loop {
            match child.try_wait()? {
                Some(status) => break (status, false),
                None => {
                    if start.elapsed() >= dur {
                        let _ = child.kill();
                        let status = child.wait().context("waiting after kill")?;
                        break (status, true);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    } else {
        let status = child.wait().context("waiting for command")?;
        (status, false)
    };

    let stdout = stdout_thread.join().expect("stdout reader panicked");
    let stderr = stderr_thread.join().expect("stderr reader panicked");

    Ok(RunResponse {
        exit_code: if timed_out {
            -1
        } else {
            status.code().unwrap_or(-1)
        },
        stdout: base64_encode(&stdout),
        stderr: base64_encode(&stderr),
        timed_out,
    })
}

// ---------------------------------------------------------------------------
// SSH agent relay
// ---------------------------------------------------------------------------

/// Listen on the SSH_AUTH_SOCK Unix socket and relay each connection
/// to the ssh-agent on the local machine via WebSocket through the git
/// HTTP server.
async fn run_ssh_agent_listener(
    relay: std::sync::Arc<tokio::sync::Mutex<Option<SshRelayConfig>>>,
) -> Result<()> {
    let sock_path = SSH_AGENT_SOCK_PATH;
    let listener = tokio::net::UnixListener::bind(sock_path)
        .with_context(|| format!("binding {sock_path}"))?;

    // Make the socket accessible to all users in the container.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o666))
            .context("chmod ssh-agent socket")?;
    }

    loop {
        let (stream, _) = listener.accept().await.context("accepting connection")?;
        let relay = relay.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_ssh_agent_connection(stream, relay).await {
                eprintln!("ssh-agent relay connection failed: {e:#}");
            }
        });
    }
}

/// Bridge a single SSH agent connection to the local machine via WebSocket.
async fn handle_ssh_agent_connection(
    stream: tokio::net::UnixStream,
    relay: std::sync::Arc<tokio::sync::Mutex<Option<SshRelayConfig>>>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite;

    let config = relay
        .lock()
        .await
        .clone()
        .context("ssh-agent relay not configured (tunnel not ready yet?)")?;

    // Connect to the git HTTP server's /ssh-agent WebSocket endpoint.
    let ws_url = format!(
        "{}/ssh-agent",
        config
            .url
            .replace("http://", "ws://")
            .replace("https://", "wss://")
    );
    let ws_request = tungstenite::http::Request::builder()
        .uri(&ws_url)
        .header("Authorization", format!("Bearer {}", config.token))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .header("Host", "localhost")
        .body(())
        .context("building WebSocket request")?;

    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_request)
        .await
        .context("connecting to ssh-agent relay WebSocket")?;

    use futures_util::{SinkExt, StreamExt};

    let (mut ws_write, mut ws_read) = ws_stream.split();
    let (mut unix_read, mut unix_write) = tokio::io::split(stream);

    // Unix socket -> WebSocket (via channel, since we need select!)
    let (unix_tx, mut unix_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let reader_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 16384];
        loop {
            let n = match unix_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if unix_tx.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            ws_msg = ws_read.next() => {
                match ws_msg {
                    Some(Ok(tungstenite::Message::Binary(data)))
                        if unix_write.write_all(&data).await.is_err() =>
                    {
                        break;
                    }
                    Some(Ok(tungstenite::Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            unix_data = unix_rx.recv() => {
                match unix_data {
                    Some(data) => {
                        if ws_write.send(tungstenite::Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    reader_task.abort();
    if let Err(e) = ws_write.send(tungstenite::Message::Close(None)).await {
        eprintln!("ssh-agent relay: failed to send close frame: {e}");
    }
    Ok(())
}
