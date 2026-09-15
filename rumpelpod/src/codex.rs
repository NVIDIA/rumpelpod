// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `rumpel codex`: host-side CLI plus the daemon-side WebSocket
//! handler that owns the per-pod codex screen session.
//!
//! Flow:
//!   - Host runs `rumpel codex foo` and immediately opens one WebSocket
//!     against the daemon's `/pod/codex/foo` route via its Unix socket.
//!   - The daemon attaches to an existing frontend without consulting pod
//!     state. If no frontend exists, it builds the spawn parameters from its
//!     live cached pod connection and starts the frontend.
//!   - Pod launch or reconnect happens only when neither path is available.
//!     The TUI survives Ctrl-a d, so subsequent invocations
//!     reattach and replay the previous conversation instead of
//!     landing on the welcome screen. A replacement frontend resumes the last
//!     remote thread, while the first frontend starts a new one.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use log::trace;
use serde::Deserialize;
use tokio_tungstenite::tungstenite;
use url::Url;

use crate::cli::CodexCommand;
use crate::config::load_json_config;
use crate::daemon::protocol::ClientContext;
use crate::daemon::{self, DaemonServer};
use crate::enter::{confirm_pod_creation, find_local_codex_cli, launch_pod};
use crate::git::get_repo_root;
use crate::pty_attach;
use crate::pty_session::{serve_ws_session_with_params, SessionSpec};

const CODEX_PROXY_TOKEN_ENV: &str = "RUMPELPOD_CODEX_PROXY_TOKEN";
type WsServerRequest = tungstenite::handshake::server::Request;
type WsServerResponse = tungstenite::handshake::server::Response;
type WsServerErrorResponse = tungstenite::handshake::server::ErrorResponse;

pub fn codex(cmd: &CodexCommand) -> Result<()> {
    let repo_root = get_repo_root()?;
    let socket_path = daemon::socket_path()?;
    let codex_bin = find_local_codex_cli();

    // form_urlencoded escapes the repo path so absolute paths with
    // slashes survive the URL.  Pod names are ASCII-only per the
    // PodName validator, so the path segment does not need escaping.
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("repo_path", &repo_root.to_string_lossy());
    if let Some(codex_bin) = &codex_bin {
        query.append_pair("codex_cli_path", &codex_bin.to_string_lossy());
    }
    query.append_pair(
        "no_dangerously_bypass_approvals_and_sandbox",
        if cmd.no_dangerously_bypass_approvals_and_sandbox {
            "true"
        } else {
            "false"
        },
    );
    let query = query.finish();
    let path = format!("/pod/codex/{}?{query}", cmd.name);

    match attach_codex(&socket_path, &path, cmd.args.clone()) {
        Ok(outcome) => return finish_codex_attachment(outcome),
        Err(error)
            if error
                .downcast_ref::<pty_attach::SessionUnavailable>()
                .is_some() =>
        {
            trace!("Codex frontend needs pod preparation");
        }
        Err(error) => return Err(error),
    }

    if codex_bin.is_none() {
        return Err(anyhow::anyhow!(
            "codex CLI not found in PATH. Install it from https://github.com/openai/codex"
        ));
    }

    let host_override = cmd.container_config.resolve_host()?;
    confirm_pod_creation(&cmd.name, &repo_root, cmd.create)?;
    launch_pod(
        &cmd.name,
        host_override,
        cmd.container_config.devcontainer.clone(),
    )?;

    let outcome = attach_codex(&socket_path, &path, cmd.args.clone()).map_err(|error| {
        if error
            .downcast_ref::<pty_attach::SessionUnavailable>()
            .is_some()
        {
            anyhow::anyhow!("pod connection remained unavailable after launch")
        } else {
            error
        }
    })?;
    finish_codex_attachment(outcome)
}

fn attach_codex(
    socket_path: &std::path::Path,
    path: &str,
    extra_args: Vec<String>,
) -> Result<pty_attach::AttachOutcome> {
    pty_attach::attach(
        pty_attach::PtyTransport::Unix {
            socket: socket_path.to_path_buf(),
        },
        path,
        // Daemon's Unix socket does not validate Authorization.
        "",
        pty_attach::WireParams::Attach {
            extra_args,
            client_context: ClientContext::current(),
        },
        None,
    )
}

fn finish_codex_attachment(outcome: pty_attach::AttachOutcome) -> Result<()> {
    match outcome {
        pty_attach::AttachOutcome::Detached => {
            eprintln!("[detached from session]");
        }
        pty_attach::AttachOutcome::SessionEnded => {}
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon-side WebSocket handler for /pod/codex/{name}
// ---------------------------------------------------------------------------

/// Build a stateless router that mounts the codex WebSocket route on
/// the daemon's main listener.  The caller (run_daemon) merges this
/// into the protocol router via `serve_daemon`'s `extra` arg.
pub fn daemon_routes(daemon: Arc<DaemonServer>) -> Router<()> {
    Router::new()
        .route("/pod/codex/{name}", any(codex_ws_handler))
        .with_state(daemon)
}

#[derive(Debug, Deserialize)]
struct CodexQuery {
    repo_path: PathBuf,
    /// Absolute path to the local codex binary, resolved by the
    /// client.  Forwarded so the daemon does not have to find codex
    /// on its own (typically narrower) PATH.
    codex_cli_path: Option<PathBuf>,
    no_dangerously_bypass_approvals_and_sandbox: bool,
}

async fn codex_ws_handler(
    ws: WebSocketUpgrade,
    State(daemon): State<Arc<DaemonServer>>,
    Path(name): Path<String>,
    Query(q): Query<CodexQuery>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        let session_name = codex_session_name(&q.repo_path, &name);
        let sessions = daemon.pty_sessions();
        let context_daemon = daemon.clone();
        serve_ws_session_with_params(
            socket,
            sessions,
            session_name,
            move |client_context| context_daemon.remember_client_context(client_context),
            move |extra_args| {
                tokio::task::block_in_place(|| {
                    build_codex_spec(
                        &daemon,
                        &q.repo_path,
                        &name,
                        q.codex_cli_path.as_deref(),
                        q.no_dangerously_bypass_approvals_and_sandbox,
                        extra_args,
                    )
                })
            },
        )
        .await
    })
}

fn build_codex_spec(
    daemon: &Arc<DaemonServer>,
    repo_path: &std::path::Path,
    pod_name: &str,
    codex_bin: Option<&std::path::Path>,
    no_dangerously_bypass_approvals_and_sandbox: bool,
    extra_args: Vec<String>,
) -> Result<Option<SessionSpec>> {
    let Some((container_url, container_token, codex_state)) =
        daemon.connected_codex_pod(repo_path, pod_name)
    else {
        return Ok(None);
    };
    let codex_bin = codex_bin.ok_or_else(|| {
        anyhow::anyhow!(
            "codex CLI not found in PATH. Install it from https://github.com/openai/codex"
        )
    })?;

    let json_config = load_json_config(repo_path)?;
    let bypass = !no_dangerously_bypass_approvals_and_sandbox
        && json_config.codex.dangerously_bypass_approvals_and_sandbox;

    // A non-empty state came from the pod's live event connection and means
    // its Codex app server already ran with the credentials copied before the
    // first frontend was created. A live local frontend bypasses this builder
    // entirely, even when its pod connection is temporarily unavailable.
    if codex_state.is_none() {
        let pod = crate::pod::PodClient::new_with_timeout(
            &container_url,
            &container_token,
            Duration::from_secs(30),
        )?;
        write_codex_credentials(&pod)?;
    }

    // Bind a per-pod loopback proxy that forwards to the pod's /codex.
    // The codex TUI dials it via `--remote`; we cannot use a Unix
    // socket here because we do not control the codex CLI.
    let proxy =
        daemon.ensure_codex_proxy(repo_path, pod_name, container_url, container_token, bypass)?;

    let remote_url = format!("ws://127.0.0.1:{}", proxy.port);
    let mut cmd = vec![
        codex_bin.to_string_lossy().into_owned(),
        "--remote".to_string(),
        remote_url,
        "--remote-auth-token-env".to_string(),
        CODEX_PROXY_TOKEN_ENV.to_string(),
    ];
    // Codex starts a new remote thread when there is nothing to resume, so every
    // spawned frontend can use the same command and survive incomplete state
    // notifications or an app-server restart.
    cmd.push("resume".to_string());
    cmd.push("--last".to_string());
    cmd.extend(extra_args);

    Ok(Some(SessionSpec {
        name: codex_session_name(repo_path, pod_name),
        cmd,
        workdir: None,
        env: vec![format!("{CODEX_PROXY_TOKEN_ENV}={}", proxy.token)],
    }))
}

pub(crate) fn codex_session_name(repo_path: &std::path::Path, pod_name: &str) -> String {
    let repo_path = repo_path.display();
    format!("codex:{repo_path}:{pod_name}")
}

/// Copy the local machine's codex credentials into the pod.
///
/// Builds a tar of ~/.codex/auth.json (and config.toml if present) on
/// the local machine, streams it through PUT /agent-files/codex.  The
/// user is expected to have run `codex login` beforehand.
fn write_codex_credentials(pod: &crate::pod::PodClient) -> Result<()> {
    let local_home = dirs::home_dir().context("could not determine home directory")?;
    let auth_path = local_home.join(".codex/auth.json");
    if !auth_path.exists() {
        return Err(anyhow::anyhow!(
            "no codex credentials found at ~/.codex/auth.json. Run `codex login` first."
        ));
    }

    let codex_dir = local_home.join(".codex");
    let entries: Vec<(String, std::path::PathBuf)> = vec![
        (".codex/auth.json".to_string(), auth_path.clone()),
        (
            ".codex/config.toml".to_string(),
            codex_dir.join("config.toml"),
        ),
    ]
    .into_iter()
    .filter(|(_, p)| p.exists())
    .collect();

    let (read_end, write_end) = std::io::pipe().context("creating pipe for codex tar")?;
    let handle = std::thread::spawn(move || -> Result<()> {
        let mut archive = tar::Builder::new(write_end);
        for (rel, src) in &entries {
            archive
                .append_path_with_name(src, rel)
                .with_context(|| format!("archiving {rel}"))?;
        }
        archive.into_inner().context("finalizing codex tar")?;
        Ok(())
    });

    pod.put_agent_files("codex", read_end, None)
        .context("uploading codex credentials")?;
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("codex tar thread panicked"))??;
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket proxy (runs inside the daemon process)
// ---------------------------------------------------------------------------

/// Accept loop for the daemon-managed codex WebSocket proxy.
///
/// Each incoming connection from the local codex TUI is forwarded to
/// the pod server's `/codex` endpoint.
///
/// `ready_tx` fires once the accept loop is running so callers know
/// the proxy is actually processing connections (not just bound).
pub async fn run_codex_proxy(
    listener: tokio::net::TcpListener,
    container_url: String,
    container_token: String,
    client_token: String,
    bypass: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::SyncSender<()>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    let _ = ready_tx.send(());
    loop {
        let stream = tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        eprintln!("codex proxy: accept error: {e}");
                        continue;
                    }
                }
            }
            _ = cancel_rx.changed() => break,
        };
        let url = container_url.clone();
        let container_token = container_token.clone();
        let client_token = client_token.clone();
        let bypass = bypass.load(Ordering::Relaxed);
        tokio::spawn(async move {
            if let Err(e) =
                proxy_connection(stream, &url, &container_token, &client_token, bypass).await
            {
                eprintln!("codex proxy: connection error: {e:#}");
            }
        });
    }
}

async fn proxy_connection(
    stream: tokio::net::TcpStream,
    container_url: &str,
    container_token: &str,
    client_token: &str,
    bypass: bool,
) -> Result<()> {
    let expected_auth = format!("Bearer {client_token}");
    let client_ws =
        tokio_tungstenite::accept_hdr_async(stream, CodexProxyAuthCallback { expected_auth })
            .await
            .context("accepting WebSocket from codex TUI")?;

    // The pod server may not be ready yet (e.g. container-serve is
    // still starting). Retry the upstream connection briefly so a
    // slow container start does not cause a permanent failure.
    let mut server_ws = None;
    for _ in 0..50 {
        let request = build_pod_ws_request(container_url, container_token, bypass)?;
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _)) => {
                server_ws = Some(ws);
                break;
            }
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    let server_ws = server_ws
        .ok_or_else(|| anyhow::anyhow!("could not connect to pod server /codex after retries"))?;

    let (mut client_write, mut client_read) = client_ws.split();
    let (mut server_write, mut server_read) = server_ws.split();

    loop {
        tokio::select! {
            msg = client_read.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        if server_write.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            msg = server_read.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        if client_write.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    Ok(())
}

struct CodexProxyAuthCallback {
    expected_auth: String,
}

impl tungstenite::handshake::server::Callback for CodexProxyAuthCallback {
    fn on_request(
        self,
        request: &WsServerRequest,
        response: WsServerResponse,
    ) -> std::result::Result<WsServerResponse, WsServerErrorResponse> {
        validate_codex_proxy_auth(request, response, &self.expected_auth)
    }
}

// Tungstenite's callback trait fixes this response type.
#[allow(clippy::result_large_err)]
fn validate_codex_proxy_auth(
    request: &WsServerRequest,
    response: WsServerResponse,
    expected_auth: &str,
) -> std::result::Result<WsServerResponse, WsServerErrorResponse> {
    let header = request
        .headers()
        .get(tungstenite::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    match header {
        Some(value) if value == expected_auth => Ok(response),
        Some(_) | None => Err(unauthorized_ws_response()),
    }
}

fn unauthorized_ws_response() -> WsServerErrorResponse {
    let mut response = tungstenite::http::Response::new(Some("unauthorized".to_string()));
    *response.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
    response
}

fn build_pod_ws_request(
    container_url: &str,
    token: &str,
    bypass: bool,
) -> Result<tungstenite::http::Request<()>> {
    let mut ws_url = Url::parse(container_url).context("parsing container URL")?;
    let scheme = match ws_url.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => return Err(anyhow::anyhow!("unexpected scheme: {other}")),
    };
    ws_url
        .set_scheme(scheme)
        .expect("ws/wss are always valid schemes");
    ws_url.set_path("/codex");
    ws_url
        .query_pairs_mut()
        .append_pair("bypass", if bypass { "true" } else { "false" });

    let host = match ws_url.port() {
        Some(port) => format!("{}:{port}", ws_url.host_str().unwrap_or("localhost")),
        None => ws_url.host_str().unwrap_or("localhost").to_string(),
    };
    tungstenite::http::Request::builder()
        .uri(ws_url.as_str())
        .header("Host", &host)
        .header("Authorization", format!("Bearer {token}"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .context("building WebSocket request for pod server")
}
