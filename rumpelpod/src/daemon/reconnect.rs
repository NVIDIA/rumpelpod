// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-pod event listeners that maintain SSE connections to pod servers
//! and broadcast reconnection status to PTY clients.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::debug;
use retry::delay::jitter;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};

use crate::async_runtime::RUNTIME;
use crate::config::Host;
use crate::daemon::host_connection::{HostConnectionRegistry, HostKey, HostStatus};
use crate::pod::types::{ClaudeState, CodexState};

const INITIAL_DELAY: Duration = Duration::from_secs(1);
/// Cap retry backoff low so the user's terminal reconnects promptly once
/// the host is reachable again.
const MAX_DELAY: Duration = Duration::from_secs(5);

/// Events streamed to clients waiting for a pod reconnection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReconnectEvent {
    /// The daemon is attempting to reconnect (host or pod).
    Attempting,
    /// The host connection has been restored; now connecting to the pod.
    HostConnected,
    /// The pod event endpoint confirmed the connection (state event received).
    Connected,
    /// A reconnection attempt failed; the daemon will retry.
    Failed { error: String },
    /// The pod was intentionally stopped; the client should exit.
    Stopped,
}

struct PodState {
    host_key: HostKey,
    tx: broadcast::Sender<ReconnectEvent>,
    stop: Arc<AtomicBool>,
    claude_state: Arc<Mutex<Option<ClaudeState>>>,
    codex_state: Arc<Mutex<Option<CodexState>>>,
    _thread: std::thread::JoinHandle<()>,
}

/// Manages persistent SSE connections to each running pod's `/events`
/// endpoint.  When a connection drops the manager reconnects (waiting
/// for the host connection first on remote hosts) and broadcasts
/// status events so PTY clients know when the pod is reachable again.
pub struct PodEventManager {
    host_connections: Arc<HostConnectionRegistry>,
    pods: Mutex<HashMap<(PathBuf, String), PodState>>,
}

impl PodEventManager {
    pub fn new(host_connections: Arc<HostConnectionRegistry>) -> Self {
        Self {
            host_connections,
            pods: Mutex::new(HashMap::new()),
        }
    }

    /// Start (or restart) the event listener for a pod.
    pub fn start(
        &self,
        repo_path: PathBuf,
        pod_name: String,
        container_url: String,
        token: String,
        host: Host,
    ) {
        let key = (repo_path, pod_name.clone());
        let mut pods = self.pods.lock().unwrap();

        // Stop existing listener if any.
        if let Some(old) = pods.remove(&key) {
            old.stop.store(true, Ordering::SeqCst);
        }

        let (tx, _) = broadcast::channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let claude_state = Arc::new(Mutex::new(None));
        let codex_state = Arc::new(Mutex::new(None));
        let host_key = HostKey::from_host(&host);

        let thread = {
            let tx = tx.clone();
            let stop = stop.clone();
            let claude_state = claude_state.clone();
            let codex_state = codex_state.clone();
            let host_connections = self.host_connections.clone();
            std::thread::Builder::new()
                .name(format!("pod-events-{pod_name}"))
                .spawn(move || {
                    pod_event_loop(
                        host_connections,
                        host,
                        container_url,
                        token,
                        stop,
                        tx,
                        claude_state,
                        codex_state,
                    );
                })
                .expect("failed to spawn pod event listener thread")
        };

        pods.insert(
            key,
            PodState {
                host_key,
                tx,
                stop,
                claude_state,
                codex_state,
                _thread: thread,
            },
        );
    }

    /// Stop the event listener for a pod.
    ///
    /// Broadcasts a `Stopped` event to any subscribers before tearing
    /// down the listener, so clients waiting for reconnection know the
    /// pod was intentionally stopped and should exit.
    pub fn stop(&self, repo_path: &Path, pod_name: &str) {
        let key = (repo_path.to_path_buf(), pod_name.to_string());
        let mut pods = self.pods.lock().unwrap();
        if let Some(state) = pods.remove(&key) {
            let _ = state.tx.send(ReconnectEvent::Stopped);
            state.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Subscribe to reconnection events for a pod.
    pub fn subscribe(
        &self,
        repo_path: &Path,
        pod_name: &str,
    ) -> Option<broadcast::Receiver<ReconnectEvent>> {
        let key = (repo_path.to_path_buf(), pod_name.to_string());
        let pods = self.pods.lock().unwrap();
        pods.get(&key).map(|state| state.tx.subscribe())
    }

    /// Read the last known Claude Code session state for a pod.
    pub fn claude_state(&self, repo_path: &Path, pod_name: &str) -> Option<ClaudeState> {
        let key = (repo_path.to_path_buf(), pod_name.to_string());
        let pods = self.pods.lock().unwrap();
        pods.get(&key)
            .and_then(|state| *state.claude_state.lock().unwrap())
    }

    /// Read the last known Codex session state for a pod.
    pub fn codex_state(&self, repo_path: &Path, pod_name: &str) -> Option<CodexState> {
        let key = (repo_path.to_path_buf(), pod_name.to_string());
        let pods = self.pods.lock().unwrap();
        pods.get(&key)
            .and_then(|state| *state.codex_state.lock().unwrap())
    }

    /// Called by the daemon's central host-event reader on
    /// `Connected(host)`.  Broadcasts `HostConnected` to every pod on
    /// that host so PTY clients know the host is back; per-pod loops
    /// then converge on their own backoff.
    pub fn notify_host_connected(&self, host: &HostKey) {
        let pods = self.pods.lock().unwrap();
        for state in pods.values() {
            if &state.host_key == host {
                let _ = state.tx.send(ReconnectEvent::HostConnected);
            }
        }
    }

    /// Called by the daemon's central host-event reader on
    /// `Disconnected(host)`.  Broadcasts `Attempting` so PTY clients
    /// see the reconnect spinner.
    pub fn notify_host_disconnected(&self, host: &HostKey) {
        let pods = self.pods.lock().unwrap();
        for state in pods.values() {
            if &state.host_key == host {
                let _ = state.tx.send(ReconnectEvent::Attempting);
            }
        }
    }
}

/// Greeting payload parsed from the initial `state` event.
struct GreetingState {
    claude: Option<ClaudeState>,
    codex: Option<CodexState>,
}

/// Try to connect to the pod's /events endpoint and read the initial
/// `state` event.  Returns the reader positioned after the greeting
/// together with agent session states from the greeting payload.
fn connect_pod_events(
    url: &str,
    token: &str,
) -> Result<(BufReader<reqwest::blocking::Response>, GreetingState), anyhow::Error> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(None)
        .build()
        .expect("failed to build reqwest client");

    let response = client
        .get(format!("{url}/events"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .map_err(|e| anyhow::anyhow!("connecting to pod events: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(anyhow::anyhow!("pod events returned {status}"));
    }

    let mut reader = BufReader::new(response);

    // Wait for the `state` event (the pod's hello after pushing branches).
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| anyhow::anyhow!("reading pod event stream: {e}"))?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "pod event stream closed before state event"
            ));
        }
        if line.trim() == "event: state" {
            // Consume the data line and blank separator.
            let mut data_line = String::new();
            reader.read_line(&mut data_line).ok();
            let mut blank = String::new();
            reader.read_line(&mut blank).ok();

            let greeting = parse_greeting_state(&data_line);
            return Ok((reader, greeting));
        }
    }
}

/// Extract agent states from the greeting data line (e.g.
/// `data: {"claude_state":"processing","codex_state":null}`).
fn parse_greeting_state(data_line: &str) -> GreetingState {
    let Some(json_str) = data_line.trim().strip_prefix("data: ") else {
        return GreetingState {
            claude: None,
            codex: None,
        };
    };
    let Ok(obj) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return GreetingState {
            claude: None,
            codex: None,
        };
    };
    let claude = obj
        .get("claude_state")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .flatten();
    let codex = obj
        .get("codex_state")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .flatten();
    GreetingState { claude, codex }
}

/// Background loop that maintains the SSE connection to a pod.
///
/// Reads the greeting (with claude_state) and then continues reading
/// events.  `event: claude_state` payloads update the shared state so
/// the daemon can report it in list_pods.
#[allow(clippy::too_many_arguments)]
fn pod_event_loop(
    host_connections: Arc<HostConnectionRegistry>,
    host: Host,
    container_url: String,
    token: String,
    stop: Arc<AtomicBool>,
    tx: broadcast::Sender<ReconnectEvent>,
    claude_state: Arc<Mutex<Option<ClaudeState>>>,
    codex_state: Arc<Mutex<Option<CodexState>>>,
) {
    let apply_greeting = |g: GreetingState| {
        *claude_state.lock().unwrap() = g.claude;
        *codex_state.lock().unwrap() = g.codex;
    };

    // Resolve the host connection once and follow its liveness.
    // `get_or_create` starts (or reuses) the single per-host monitor
    // that owns probing and reconnection; this loop never probes the
    // host itself, it only waits for the monitor to report Connected.
    // Holding the connection keeps that monitor alive for the pod.
    let host_conn = loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match host_connections.get_or_create(&host) {
            Ok(conn) => break conn,
            Err(e) => {
                debug!("getting host connection failed: {e:#}");
                std::thread::sleep(jitter(INITIAL_DELAY));
            }
        }
    };
    let mut host_rx = host_conn.subscribe();

    // Backoff for pod-endpoint failures while the host is up (e.g. the
    // pod server is still starting).  Reset on every successful connect.
    let mut backoff = INITIAL_DELAY;

    // (Re)establish the /events connection, read until it drops, repeat.
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }

        // Gate: wait for the host before touching the pod URL.  Nudge
        // the monitor so it checks now instead of waiting out its
        // heartbeat.  For localhost the status is permanently Connected,
        // so this returns immediately.
        let _ = tx.send(ReconnectEvent::Attempting);
        host_conn.request_probe();
        if !wait_for_host(&mut host_rx, &stop) {
            return;
        }
        let _ = tx.send(ReconnectEvent::HostConnected);

        // Try the pod endpoint.  A failure here is ambiguous (pod still
        // starting vs host actually down), so nudge the monitor and
        // retry after a short backoff; if the host is truly down the
        // gate above catches it on the next iteration.
        let mut reader = match connect_pod_events(&container_url, &token) {
            Ok((reader, greeting)) => {
                apply_greeting(greeting);
                let _ = tx.send(ReconnectEvent::Connected);
                backoff = INITIAL_DELAY;
                reader
            }
            Err(e) => {
                debug!("pod event connection failed: {e:#}");
                let _ = tx.send(ReconnectEvent::Failed {
                    error: format!("{e:#}"),
                });
                host_conn.request_probe();
                std::thread::sleep(jitter(backoff));
                backoff = std::cmp::min(backoff.saturating_mul(2), MAX_DELAY);
                continue;
            }
        };

        // Read events until the connection drops.  Parse
        // `event: claude_state` / `event: codex_state` lines to track
        // the current session state.
        let mut pending_event: Option<String> = None;
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                // Connection lost -- break to the gate and reconnect.
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if let Some(event_type) = trimmed.strip_prefix("event: ") {
                        pending_event = Some(event_type.to_string());
                    } else if let Some(data) = trimmed.strip_prefix("data: ") {
                        match pending_event.as_deref() {
                            Some("claude_state") => {
                                if let Ok(cs) = serde_json::from_str::<Option<ClaudeState>>(data) {
                                    *claude_state.lock().unwrap() = cs;
                                }
                            }
                            Some("codex_state") => {
                                if let Ok(xs) = serde_json::from_str::<Option<CodexState>>(data) {
                                    *codex_state.lock().unwrap() = xs;
                                }
                            }
                            _ => {}
                        }
                        pending_event = None;
                    } else if trimmed.is_empty() {
                        pending_event = None;
                    }
                }
            }
        }
    }
}

/// Block until the host reports `Connected`, returning false if the
/// listener was asked to stop.  Wakes periodically to re-check `stop`,
/// which is not awaitable.
fn wait_for_host(host_rx: &mut watch::Receiver<HostStatus>, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        if *host_rx.borrow() == HostStatus::Connected {
            return true;
        }
        let _ = RUNTIME.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), host_rx.changed()).await
        });
    }
}
