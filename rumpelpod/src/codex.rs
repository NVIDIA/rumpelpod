// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `rumpel codex`: host-side CLI plus the daemon-side WebSocket
//! handler that owns the per-pod codex screen session.
//!
//! Flow:
//!   - Host runs `rumpel codex foo`: resolves the local codex binary,
//!     launches the pod, writes codex credentials into it, then opens
//!     one WebSocket against the daemon's `/pod/codex/foo` route via
//!     the existing Unix socket, passing the resolved codex path as a
//!     query parameter.
//!   - Daemon's handler starts a per-pod loopback proxy to the pod
//!     server's /codex (the codex TUI's `--remote` arg dials it), and
//!     spawns or attaches a screen session whose child is the codex
//!     TUI.  The TUI survives Ctrl-a d, so subsequent invocations
//!     reattach and replay the previous conversation instead of
//!     landing on the welcome screen.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite;
use url::Url;

use crate::cli::CodexCommand;
use crate::config::load_json_config;
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
    let host_override = cmd.host_args.resolve()?;
    let json_config = load_json_config(&repo_root)?;

    // Resolve the local codex binary on the client side: the daemon
    // (often a systemd user service) may not have codex on its own
    // PATH.  The path is forwarded to the daemon both for image
    // preparation (via launch_pod) and for the TUI launch (via the
    // /pod/codex query string below).
    let codex_bin = find_local_codex_cli().ok_or_else(|| {
        anyhow::anyhow!(
            "codex CLI not found in PATH. Install it from https://github.com/openai/codex"
        )
    })?;

    confirm_pod_creation(&cmd.name, &repo_root, cmd.create)?;

    let result = launch_pod(&cmd.name, host_override)?;

    let pod = crate::pod::PodClient::connect(&result.container_url, &result.container_token)?;
    write_codex_credentials(&pod)?;

    // CLI --no-dangerously-bypass-approvals-and-sandbox wins over the config setting.
    let bypass = !cmd.no_dangerously_bypass_approvals_and_sandbox
        && json_config.codex.dangerously_bypass_approvals_and_sandbox;
    let mut extra_args: Vec<String> = Vec::new();
    if bypass {
        extra_args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    }
    extra_args.extend(cmd.args.iter().cloned());

    let socket_path = daemon::socket_path()?;
    // form_urlencoded escapes the repo path so absolute paths with
    // slashes survive the URL.  Pod names are ASCII-only per the
    // PodName validator, so the path segment does not need escaping.
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("repo_path", &repo_root.to_string_lossy())
        .append_pair("codex_cli_path", &codex_bin.to_string_lossy())
        .finish();
    let path = format!("/pod/codex/{}?{query}", cmd.name);

    let outcome = pty_attach::attach(
        pty_attach::PtyTransport::Unix {
            socket: socket_path,
        },
        &path,
        // Daemon's Unix socket does not validate Authorization.
        "",
        pty_attach::WireParams::Attach { extra_args },
        None,
    )?;

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
    codex_cli_path: PathBuf,
}

async fn codex_ws_handler(
    ws: WebSocketUpgrade,
    State(daemon): State<Arc<DaemonServer>>,
    Path(name): Path<String>,
    Query(q): Query<CodexQuery>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        let spec = match build_codex_spec(&daemon, &q.repo_path, &name, &q.codex_cli_path).await {
            Ok(spec) => spec,
            Err(e) => {
                eprintln!("codex ws handler: {e:#}");
                return;
            }
        };
        serve_ws_session_with_params(socket, daemon.pty_sessions(), spec).await
    })
}

async fn build_codex_spec(
    daemon: &Arc<DaemonServer>,
    repo_path: &std::path::Path,
    pod_name: &str,
    codex_bin: &std::path::Path,
) -> Result<SessionSpec> {
    // Look up token + container URL from the daemon's existing pod
    // state.  The host calls launch_pod before opening the WS, so
    // both must be populated.
    let token = daemon
        .pod_token(repo_path, pod_name)?
        .with_context(|| format!("no token for pod '{pod_name}'"))?;
    let container_url = daemon
        .pod_container_url(repo_path, pod_name)
        .with_context(|| format!("no container URL for pod '{pod_name}' (pod not running?)"))?;

    // Bind a per-pod loopback proxy that forwards to the pod's /codex.
    // The codex TUI dials it via `--remote`; we cannot use a Unix
    // socket here because we do not control the codex CLI.
    let proxy = tokio::task::block_in_place(|| {
        daemon.ensure_codex_proxy(repo_path, pod_name, container_url, token)
    })?;

    let remote_url = format!("ws://127.0.0.1:{}", proxy.port);
    let cmd = vec![
        codex_bin.to_string_lossy().into_owned(),
        "--remote".to_string(),
        remote_url,
        "--remote-auth-token-env".to_string(),
        CODEX_PROXY_TOKEN_ENV.to_string(),
    ];

    Ok(SessionSpec {
        name: codex_session_name(repo_path, pod_name),
        cmd,
        workdir: None,
        env: vec![format!("{CODEX_PROXY_TOKEN_ENV}={}", proxy.token)],
    })
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
        tokio::spawn(async move {
            if let Err(e) = proxy_connection(stream, &url, &container_token, &client_token).await {
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
        let request = build_pod_ws_request(container_url, container_token)?;
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
