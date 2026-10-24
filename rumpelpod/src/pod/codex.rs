// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bridge the host TUI to Codex's native daemon over its Unix socket.
//!
//! Codex owns installation, process supervision, updates, and remote control.
//! Rumpelpod starts it on demand and restores it when an initialized pod starts.
//! A separate connection tracks thread status even without an attached TUI.

mod snapshot;
#[cfg(test)]
mod status_tests;

pub(super) use snapshot::{archive as archive_state, reset_fork_identity, SNAPSHOT_SOURCE_FILE};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_tungstenite::{tungstenite, WebSocketStream};

use crate::codex::{codex_websocket_config, CODEX_MAX_MESSAGE_SIZE};

use super::server::PodServerState;
use super::types::CodexState;

// Native startup may wait for another lifecycle operation's lock, including a
// graceful restart.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(120);
// Keep activation outside agent snapshots: installing Codex or copying another
// pod's history must not opt a pod into running a background daemon.
const DAEMON_ENABLED_FILE: &str = "/opt/rumpelpod/codex-daemon-enabled";

// Unlike activation, initialized config follows conversations through forks and
// recreation so native config edits are not replaced by launch defaults.
const CONFIG_INITIALIZED_FILE: &str = ".codex/.rumpelpod-config-initialized";

/// The socket path remains stable across native restarts and package updates.
pub type CodexAppServer = Arc<Mutex<Option<PathBuf>>>;

pub fn new_codex_app_server() -> CodexAppServer {
    Arc::new(Mutex::new(None))
}

#[derive(Deserialize)]
pub struct CodexQuery {
    bypass: bool,
}

pub async fn codex_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<super::server::PodServerState>,
    Query(query): Query<CodexQuery>,
) -> Response {
    ws.max_message_size(CODEX_MAX_MESSAGE_SIZE)
        .max_frame_size(CODEX_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_codex_proxy(socket, state, query.bypass))
}

async fn handle_codex_proxy(
    mut client_ws: WebSocket,
    state: super::server::PodServerState,
    bypass: bool,
) {
    let repo_path = match state.repo_path.lock().await.clone() {
        Some(repo_path) => repo_path,
        None => {
            eprintln!("codex: repo_path not set yet");
            let _ = client_ws
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: "repo_path not set yet".into(),
                })))
                .await;
            return;
        }
    };

    let socket_path = match start_daemon(&state, &repo_path, Some(bypass)).await {
        Ok(socket_path) => socket_path,
        Err(e) => {
            eprintln!("codex: failed to start native daemon: {e:#}");
            let reason = format!("{e:#}");
            // A WebSocket close reason is limited to 123 UTF-8 bytes. Keep the
            // actual startup error visible without making the close invalid.
            let mut end = reason.len().min(123);
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            if let Err(error) = client_ws
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: reason[..end].to_string().into(),
                })))
                .await
            {
                eprintln!("codex: reporting startup failure: {error}");
            }
            return;
        }
    };

    if let Err(e) = proxy_to_app_server(client_ws, &socket_path, bypass).await {
        eprintln!("codex: proxy error: {e:#}");
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonStartOutput {
    socket_path: PathBuf,
}

/// Delegate reuse, stale PID recovery, and readiness checks to Codex itself.
async fn start_daemon(
    state: &PodServerState,
    repo_path: &Path,
    bypass: Option<bool>,
) -> Result<PathBuf> {
    let mut socket = state.codex_app_server.lock().await;
    if let Some(bypass) = bypass {
        initialize_config(&super::server::server_home()?, bypass)?;
    }
    let output = tokio::time::timeout(
        DAEMON_START_TIMEOUT,
        Command::new(find_codex_cli()?)
            .args(["app-server", "daemon", "start"])
            .current_dir(repo_path)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("timed out starting native Codex daemon")?
    .context("running codex app-server daemon start")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let status = output.status;
        return Err(anyhow::anyhow!(
            "Codex daemon startup failed ({status}): {stderr}"
        ));
    }
    if !stderr.is_empty() {
        eprint!("codex daemon: {stderr}");
    }
    let started: DaemonStartOutput = serde_json::from_slice(&output.stdout)
        .context("parsing native Codex daemon startup response")?;
    tokio::fs::write(DAEMON_ENABLED_FILE, b"")
        .await
        .context("recording that rumpel codex enabled the native daemon")?;
    *socket = Some(started.socket_path.clone());
    if !state.codex_monitor_started.swap(true, Ordering::SeqCst) {
        tokio::spawn(codex_state_monitor(
            state.codex_state.clone(),
            state.codex_app_server.clone(),
        ));
    }
    Ok(started.socket_path)
}

/// Restore mobile availability only after `rumpel codex` has activated this
/// pod. Merely having a native installation must not start a daemon.
pub async fn restore_daemon(state: &PodServerState, repo_path: &Path) -> Result<()> {
    if Path::new(DAEMON_ENABLED_FILE)
        .try_exists()
        .context("checking whether rumpel codex enabled the native daemon")?
    {
        start_daemon(state, repo_path, None).await?;
    }
    Ok(())
}

/// Initial permission defaults come from the existing /codex?bypass request.
/// Native startup ignores general -c flags, so both local and mobile clients
/// must read these defaults from the pod's own config.
fn initialize_config(home: &Path, bypass: bool) -> Result<()> {
    let initialized = home.join(CONFIG_INITIALIZED_FILE);
    if initialized
        .try_exists()
        .context("checking Codex config initialization")?
    {
        return Ok(());
    }
    let path = home.join(".codex/config.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context("reading pod Codex config"),
    };
    let mut config = raw
        .parse::<toml_edit::DocumentMut>()
        .context("parsing pod Codex config")?;
    if bypass {
        config["approval_policy"] = toml_edit::value("never");
        config["default_permissions"] = toml_edit::value(":danger-full-access");
    }
    std::fs::create_dir_all(home.join(".codex")).context("creating pod Codex home")?;
    std::fs::write(path, config.to_string()).context("writing pod Codex permission defaults")?;
    std::fs::write(initialized, b"").context("recording Codex config initialization")?;
    Ok(())
}

/// The host may resend its original login whenever no live state is available.
/// Once the pod owns a login, native refreshes, edits and logout must survive it.
pub(super) fn preserve_credentials(home: &Path) -> Result<bool> {
    for path in [
        home.join(CONFIG_INITIALIZED_FILE),
        home.join(".codex/auth.json"),
    ] {
        if path
            .try_exists()
            .context("checking pod Codex credentials")?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn connect_app_server(socket_path: &Path) -> Result<WebSocketStream<UnixStream>> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to Codex socket {}", socket_path.display()))?;
    let (ws, _) = tokio_tungstenite::client_async_with_config(
        "ws://localhost/",
        stream,
        Some(codex_websocket_config()),
    )
    .await
    .context("handshaking with Codex daemon")?;
    Ok(ws)
}

/// The native local transport carries the same WebSocket frames as --remote.
async fn proxy_to_app_server(client_ws: WebSocket, socket_path: &Path, bypass: bool) -> Result<()> {
    let server_ws = connect_app_server(socket_path).await?;
    let (mut server_write, mut server_read) = server_ws.split();
    let (mut client_write, mut client_read) = client_ws.split();
    let mut use_server_defaults = bypass;

    loop {
        tokio::select! {
            msg = client_read.next() => {
                match msg {
                    Some(msg) => {
                        let msg = msg.context("reading WebSocket from daemon codex proxy")?;
                        let msg = use_app_server_permission_defaults(msg, &mut use_server_defaults);
                        let tung_msg = axum_to_tungstenite(msg);
                        server_write.send(tung_msg).await.context("forwarding WebSocket to codex app-server")?;
                    }
                    None => break,
                }
            }
            msg = server_read.next() => {
                match msg {
                    Some(msg) => {
                        let msg = msg.context("reading WebSocket from codex app-server")?;
                        let axum_msg = tungstenite_to_axum(msg);
                        client_write.send(axum_msg).await.context("forwarding WebSocket to daemon codex proxy")?;
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}

fn use_app_server_permission_defaults(msg: Message, use_server_defaults: &mut bool) -> Message {
    if !*use_server_defaults {
        return msg;
    }
    let Message::Text(text) = msg else {
        return msg;
    };
    let Ok(mut request) = serde_json::from_str::<serde_json::Value>(&text) else {
        // Leave malformed requests to the app-server's protocol error handling.
        return Message::Text(text);
    };
    let method = request.get("method").and_then(|method| method.as_str());
    if method == Some("thread/settings/update") {
        if [
            "approvalPolicy",
            "approvalsReviewer",
            "sandboxPolicy",
            "permissions",
        ]
        .iter()
        .any(|key| {
            request["params"]
                .get(*key)
                .is_some_and(|value| !value.is_null())
        }) {
            // Later /new requests carry this explicit choice in legacy fields.
            *use_server_defaults = false;
        }
        return Message::Text(text);
    }
    if method != Some("thread/start") {
        return Message::Text(text);
    }
    let Some(params) = request
        .get_mut("params")
        .and_then(|params| params.as_object_mut())
    else {
        return Message::Text(text);
    };
    if params
        .get("permissions")
        .is_some_and(|permissions| !permissions.is_null())
    {
        // Named profiles come from an explicit choice in the permission picker.
        *use_server_defaults = false;
        return Message::Text(text);
    }

    // Codex 0.154 sends these local defaults even without permission flags.
    // Omitting them lets the app-server choose defaults until the user makes
    // an explicit permission selection. Resumes retain their saved permissions.
    params.remove("approvalPolicy");
    params.remove("sandbox");
    Message::Text(request.to_string().into())
}

// ---------------------------------------------------------------------------
// State monitor -- dedicated connection for tracking thread status
// ---------------------------------------------------------------------------

/// Long-running task that maintains a WebSocket connection to the codex
/// app-server and updates the shared state channel from
/// `thread/status/changed` notifications.
async fn codex_state_monitor(
    tx: tokio::sync::watch::Sender<Option<super::types::CodexState>>,
    app_server: CodexAppServer,
) {
    loop {
        // Follow the rendezvous path again after native restart/update. The
        // monitor must not undo an intentional `codex app-server daemon stop`.
        let socket_path = app_server.lock().await.clone();
        if let Some(socket_path) = socket_path {
            if let Err(e) = run_codex_monitor(&tx, &socket_path).await {
                eprintln!("codex state monitor: {e:#}");
            }
            tx.send_replace(None);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Single monitoring session: connect, handshake, and read notifications
/// until the connection drops.
async fn run_codex_monitor(
    tx: &tokio::sync::watch::Sender<Option<super::types::CodexState>>,
    socket_path: &Path,
) -> Result<()> {
    let mut ws = connect_app_server(socket_path).await?;
    let mut threads = CodexThreads::default();

    // -- Initialize handshake --
    // Opt out of high-volume notifications we do not need for state
    // tracking.  thread/status/changed is NOT in this list so it will
    // be delivered.
    let init_req = serde_json::json!({
        "id": "rumpelpod-init",
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "rumpelpod-monitor",
                "version": "0.1.0"
            },
            "capabilities": {
                "optOutNotificationMethods": [
                    "item/agentMessage/delta",
                    "item/started",
                    "item/completed",
                    "turn/started",
                    "turn/completed"
                ]
            }
        }
    });
    ws.send(tungstenite::Message::Text(init_req.to_string().into()))
        .await
        .context("sending initialize")?;

    // The server may send notifications before the initialize response.
    // Keep reading until we see our response.
    loop {
        let msg = ws
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("connection closed during handshake"))?
            .context("reading handshake message")?;
        if let tungstenite::Message::Text(ref text) = msg {
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(text.as_ref()) {
                // Process any notifications that arrive before the response.
                threads.notification(&obj)?;
                threads.publish(tx);
                if obj.get("id").and_then(|v| v.as_str()) == Some("rumpelpod-init") {
                    if let Some(error) = obj.get("error") {
                        return Err(anyhow::anyhow!(
                            "Codex monitor initialization failed: {error}"
                        ));
                    }
                    break;
                }
            }
        }
    }

    ws.send(tungstenite::Message::Text(
        r#"{"method":"initialized"}"#.into(),
    ))
    .await
    .context("sending initialized")?;

    // Reconnecting does not replay status transitions. Read every loaded
    // thread, including subagents, without resuming or subscribing to them.
    let mut cursor = None;
    loop {
        let result = monitor_request(
            &mut ws,
            &mut threads,
            "thread/loaded/list",
            serde_json::json!({"cursor": cursor}),
        )
        .await?;
        let page: LoadedThreads =
            serde_json::from_value(result).context("parsing loaded Codex threads")?;
        for id in page.data {
            let result = monitor_request(
                &mut ws,
                &mut threads,
                "thread/read",
                serde_json::json!({"threadId": id, "includeTurns": false}),
            )
            .await?;
            let response: ReadThread =
                serde_json::from_value(result).context("parsing Codex thread snapshot")?;
            // Notifications observed during startup take precedence over a
            // snapshot that might have been captured before the transition.
            threads
                .threads
                .entry(response.thread.id)
                .or_insert(response.thread.status);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    threads.publish(tx);

    // -- Read notifications until disconnect --
    while let Some(msg) = ws.next().await {
        let msg = msg.context("reading notification")?;
        if let tungstenite::Message::Text(ref text) = msg {
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(text.as_ref()) {
                threads.notification(&obj)?;
                threads.publish(tx);
            }
        }
    }

    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadedThreads {
    data: Vec<String>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct ReadThread {
    thread: ThreadSnapshot,
}

#[derive(Deserialize)]
struct ThreadSnapshot {
    id: String,
    status: ThreadStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadStatusChanged {
    thread_id: String,
    status: ThreadStatus,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    Active {
        #[serde(rename = "activeFlags")]
        active_flags: Vec<ThreadActiveFlag>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum ThreadActiveFlag {
    WaitingOnApproval,
    WaitingOnUserInput,
}

#[derive(Default)]
struct CodexThreads {
    threads: HashMap<String, ThreadStatus>,
}

impl CodexThreads {
    fn notification(&mut self, msg: &serde_json::Value) -> Result<()> {
        match msg.get("method").and_then(|m| m.as_str()) {
            Some("thread/status/changed") => {
                let change: ThreadStatusChanged = serde_json::from_value(msg["params"].clone())
                    .context("parsing Codex thread status notification")?;
                self.threads.insert(change.thread_id, change.status);
            }
            Some("thread/started") => {
                let started: ReadThread = serde_json::from_value(msg["params"].clone())
                    .context("parsing Codex thread started notification")?;
                self.threads
                    .insert(started.thread.id, started.thread.status);
            }
            // This connection also receives unrelated app-server notifications
            // and responses. Only thread lifecycle messages describe activity.
            _ => {}
        }
        Ok(())
    }

    fn publish(&self, tx: &tokio::sync::watch::Sender<Option<CodexState>>) {
        let current = self.state();
        tx.send_if_modified(|state| {
            if *state == current {
                false
            } else {
                *state = current;
                true
            }
        });
    }

    fn state(&self) -> Option<CodexState> {
        let mut result = None;
        for status in self.threads.values() {
            let state = match status {
                ThreadStatus::NotLoaded => continue,
                ThreadStatus::Idle => CodexState::Idle,
                ThreadStatus::SystemError => CodexState::Error,
                ThreadStatus::Active { active_flags } => {
                    let waiting = active_flags.iter().any(|flag| match flag {
                        ThreadActiveFlag::WaitingOnApproval
                        | ThreadActiveFlag::WaitingOnUserInput => true,
                    });
                    if !waiting {
                        return Some(CodexState::Processing);
                    }
                    CodexState::Idle
                }
            };
            if result != Some(CodexState::Error) {
                result = Some(state);
            }
        }
        result
    }
}

/// Requests are sequential on this dedicated connection. Keep consuming status
/// changes while awaiting responses so startup cannot miss a finishing agent.
async fn monitor_request(
    ws: &mut WebSocketStream<UnixStream>,
    threads: &mut CodexThreads,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let request = serde_json::json!({"id": "rumpelpod-state", "method": method, "params": params});
    ws.send(tungstenite::Message::Text(request.to_string().into()))
        .await
        .with_context(|| format!("sending Codex {method}"))?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(msg) = ws.next().await {
            let msg = msg.context("reading Codex monitor response")?;
            if let tungstenite::Message::Text(text) = msg {
                let obj: serde_json::Value =
                    serde_json::from_str(&text).context("parsing Codex monitor response")?;
                if obj.get("id").and_then(|id| id.as_str()) == Some("rumpelpod-state") {
                    if let Some(error) = obj.get("error") {
                        return Err(anyhow::anyhow!("Codex {method} failed: {error}"));
                    }
                    return obj
                        .get("result")
                        .cloned()
                        .context("Codex response missing result");
                }
                threads.notification(&obj)?;
            }
        }
        Err(anyhow::anyhow!(
            "Codex monitor disconnected during {method}"
        ))
    })
    .await
    .with_context(|| format!("timed out reading Codex {method}"))?
}

// ---------------------------------------------------------------------------
// Message conversion between axum and tungstenite WebSocket types
// ---------------------------------------------------------------------------

fn axum_to_tungstenite(msg: Message) -> tungstenite::Message {
    match msg {
        Message::Text(t) => tungstenite::Message::Text(t.to_string().into()),
        Message::Binary(b) => tungstenite::Message::Binary(b.to_vec().into()),
        Message::Ping(p) => tungstenite::Message::Ping(p.to_vec().into()),
        Message::Pong(p) => tungstenite::Message::Pong(p.to_vec().into()),
        Message::Close(Some(cf)) => {
            tungstenite::Message::Close(Some(tungstenite::protocol::CloseFrame {
                code: tungstenite::protocol::frame::coding::CloseCode::from(cf.code),
                reason: cf.reason.to_string().into(),
            }))
        }
        Message::Close(None) => tungstenite::Message::Close(None),
    }
}

fn tungstenite_to_axum(msg: tungstenite::Message) -> Message {
    match msg {
        tungstenite::Message::Text(t) => Message::Text(t.to_string().into()),
        tungstenite::Message::Binary(b) => Message::Binary(b.to_vec().into()),
        tungstenite::Message::Ping(p) => Message::Ping(p.to_vec().into()),
        tungstenite::Message::Pong(p) => Message::Pong(p.to_vec().into()),
        tungstenite::Message::Close(Some(cf)) => {
            Message::Close(Some(axum::extract::ws::CloseFrame {
                code: cf.code.into(),
                reason: cf.reason.to_string().into(),
            }))
        }
        tungstenite::Message::Close(None) => Message::Close(None),
        tungstenite::Message::Frame(_) => Message::Binary(Vec::new().into()),
    }
}

// ---------------------------------------------------------------------------
// Codex CLI resolution
// ---------------------------------------------------------------------------

/// Return the path to the codex binary inside the container.
fn find_codex_cli() -> Result<PathBuf> {
    let bin_path = Path::new(crate::daemon::CODEX_CONTAINER_BIN);
    if bin_path.exists() {
        return Ok(bin_path.to_path_buf());
    }

    if let Some(found) = crate::which("codex") {
        return Ok(found);
    }

    Err(anyhow::anyhow!(
        "Codex CLI not found at {} or in PATH",
        crate::daemon::CODEX_CONTAINER_BIN
    ))
}
