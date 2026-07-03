// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-pod daemon connection state.
//!
//! A `PodConnection` owns the host-side resources that make a running
//! pod reachable from the daemon: the pod-server exec proxy, the git
//! tunnel, user-facing forwarded ports, the optional ssh-agent, and
//! the codex proxy.  It also owns the pod event stream reconnect loop.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use log::debug;
use retry::delay::jitter;
use tokio::sync::{broadcast, watch};

use crate::async_runtime::RUNTIME;
use crate::config::Host;
use crate::daemon::host_connection::{HostConnection, HostConnectionRegistry, HostKey, HostStatus};
use crate::daemon::reconnect::ReconnectEvent;
use crate::daemon::{ssh_agent_dir, CodexProxyEndpoint, CodexProxyHandle, SshAgentHandle};
use crate::pod::types::{ClaudeState, CodexState};

const INITIAL_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct PodConnectionKey {
    repo_path: PathBuf,
    pod_name: String,
}

impl PodConnectionKey {
    pub fn new(repo_path: impl Into<PathBuf>, pod_name: impl Into<String>) -> Self {
        Self {
            repo_path: repo_path.into(),
            pod_name: pod_name.into(),
        }
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    pub fn pod_name(&self) -> &str {
        &self.pod_name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodConnectionStatus {
    HostDisconnected,
    Connecting,
    Connected,
    PodDisconnected,
    Stopped,
}

#[derive(Debug, Clone)]
pub struct PodEndpoint {
    pub url: String,
    pub token: String,
}

struct PodConnectionResources {
    pod_server: Option<crate::exec_proxy::ExecProxyHandle>,
    git_tunnel: Option<crate::tunnel::TunnelHandle>,
    forwarded_ports: Option<Vec<crate::exec_proxy::ExecProxyHandle>>,
    ssh_agent: Option<SshAgentHandle>,
    codex_proxy: Option<CodexProxyHandle>,
}

impl PodConnectionResources {
    fn new() -> Self {
        Self {
            pod_server: None,
            git_tunnel: None,
            forwarded_ports: None,
            ssh_agent: None,
            codex_proxy: None,
        }
    }
}

struct EventLoopHandle {
    stop: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

pub struct PodConnection {
    key: PodConnectionKey,
    host: Mutex<Host>,
    host_key: Mutex<HostKey>,
    host_conn: Mutex<Arc<HostConnection>>,
    token: Mutex<String>,
    status: Arc<Mutex<PodConnectionStatus>>,
    tx: broadcast::Sender<ReconnectEvent>,
    claude_state: Arc<Mutex<Option<ClaudeState>>>,
    codex_state: Arc<Mutex<Option<CodexState>>>,
    resources: Mutex<PodConnectionResources>,
    event_loop: Mutex<Option<EventLoopHandle>>,
    host_connections: Arc<HostConnectionRegistry>,
}

impl PodConnection {
    fn new(
        host_connections: Arc<HostConnectionRegistry>,
        key: PodConnectionKey,
        host: Host,
        token: String,
    ) -> Result<Self> {
        let host_conn = host_connections.get_or_create(&host)?;
        let host_key = HostKey::from_host(&host);
        let initial_status = if host_conn.status() == HostStatus::Connected {
            PodConnectionStatus::PodDisconnected
        } else {
            PodConnectionStatus::HostDisconnected
        };
        let (tx, _) = broadcast::channel(64);
        Ok(Self {
            key,
            host: Mutex::new(host),
            host_key: Mutex::new(host_key),
            host_conn: Mutex::new(host_conn),
            token: Mutex::new(token),
            status: Arc::new(Mutex::new(initial_status)),
            tx,
            claude_state: Arc::new(Mutex::new(None)),
            codex_state: Arc::new(Mutex::new(None)),
            resources: Mutex::new(PodConnectionResources::new()),
            event_loop: Mutex::new(None),
            host_connections,
        })
    }

    pub fn key(&self) -> &PodConnectionKey {
        &self.key
    }

    pub fn update_host_and_token(&self, host: Host, token: String) -> Result<()> {
        let host_key = HostKey::from_host(&host);
        let should_update = *self.host_key.lock().unwrap() != host_key;
        if should_update {
            let host_conn = self.host_connections.get_or_create(&host)?;
            *self.host.lock().unwrap() = host;
            *self.host_key.lock().unwrap() = host_key;
            *self.host_conn.lock().unwrap() = host_conn;
            self.stop_event_loop();
        }
        *self.token.lock().unwrap() = token;
        Ok(())
    }

    pub fn status(&self) -> PodConnectionStatus {
        *self.status.lock().unwrap()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ReconnectEvent> {
        self.tx.subscribe()
    }

    pub fn has_event_loop(&self) -> bool {
        self.event_loop.lock().unwrap().is_some()
    }

    pub fn claude_state(&self) -> Option<ClaudeState> {
        *self.claude_state.lock().unwrap()
    }

    pub fn codex_state(&self) -> Option<CodexState> {
        *self.codex_state.lock().unwrap()
    }

    pub fn endpoint(&self) -> Option<PodEndpoint> {
        let resources = self.resources.lock().unwrap();
        let handle = resources.pod_server.as_ref()?;
        if !handle.is_alive() {
            return None;
        }
        Some(PodEndpoint {
            url: format!("http://127.0.0.1:{}", handle.port),
            token: self.token.lock().unwrap().clone(),
        })
    }

    pub fn has_alive_pod_server(&self) -> bool {
        self.resources
            .lock()
            .unwrap()
            .pod_server
            .as_ref()
            .is_some_and(|h| h.is_alive())
    }

    pub fn remove_pod_server(&self) {
        self.resources.lock().unwrap().pod_server = None;
        self.stop_event_loop();
    }

    pub fn set_pod_server(
        &self,
        host: Host,
        token: String,
        handle: crate::exec_proxy::ExecProxyHandle,
    ) -> Result<PodEndpoint> {
        self.update_host_and_token(host, token)?;
        if self.resources.lock().unwrap().pod_server.is_some() {
            self.stop_event_loop();
        }
        let url = format!("http://127.0.0.1:{}", handle.port);
        self.resources.lock().unwrap().pod_server = Some(handle);
        Ok(PodEndpoint {
            url,
            token: self.token.lock().unwrap().clone(),
        })
    }

    pub fn ensure_event_loop(&self) {
        if self.endpoint().is_none() {
            return;
        }
        let mut event_loop = self.event_loop.lock().unwrap();
        if event_loop.is_some() {
            return;
        }

        let endpoint = self.endpoint().expect("endpoint checked above");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let host_conn = self.host_conn.lock().unwrap().clone();
        let tx = self.tx.clone();
        let status = self.status.clone();
        let claude_state = self.claude_state.clone();
        let codex_state = self.codex_state.clone();
        let pod_name = self.key.pod_name.clone();

        *self.status.lock().unwrap() = PodConnectionStatus::Connecting;
        let thread = std::thread::Builder::new()
            .name(format!("pod-connection-{pod_name}"))
            .spawn(move || {
                pod_event_loop(PodEventLoop {
                    host_conn,
                    container_url: endpoint.url,
                    token: endpoint.token,
                    stop: thread_stop,
                    tx,
                    status,
                    claude_state,
                    codex_state,
                });
            })
            .expect("failed to spawn pod connection event loop");
        *event_loop = Some(EventLoopHandle {
            stop,
            _thread: thread,
        });
    }

    pub fn git_tunnel_is_alive(&self) -> bool {
        self.resources
            .lock()
            .unwrap()
            .git_tunnel
            .as_ref()
            .is_some_and(|h| h.is_alive())
    }

    pub fn remove_git_tunnel(&self) {
        self.resources.lock().unwrap().git_tunnel = None;
    }

    pub fn set_git_tunnel(&self, handle: crate::tunnel::TunnelHandle) {
        self.resources.lock().unwrap().git_tunnel = Some(handle);
    }

    pub fn has_forwarded_ports(&self) -> bool {
        self.resources.lock().unwrap().forwarded_ports.is_some()
    }

    pub fn set_forwarded_ports(&self, handles: Vec<crate::exec_proxy::ExecProxyHandle>) {
        self.resources.lock().unwrap().forwarded_ports = Some(handles);
    }

    pub fn add_forwarded_port(&self, handle: crate::exec_proxy::ExecProxyHandle) {
        self.resources
            .lock()
            .unwrap()
            .forwarded_ports
            .get_or_insert_with(Vec::new)
            .push(handle);
    }

    pub fn ensure_ssh_agent(&self) -> Result<PathBuf> {
        let pod_name = crate::daemon::protocol::PodName(self.key.pod_name.clone());
        let agent_dir = ssh_agent_dir(&self.key.repo_path, &pod_name);
        let sock_path = agent_dir.join("agent.sock");

        let mut resources = self.resources.lock().unwrap();
        let need_start = if let Some(handle) = resources.ssh_agent.as_mut() {
            match handle.child.try_wait() {
                Ok(Some(_)) => {
                    resources.ssh_agent = None;
                    true
                }
                Ok(None) => false,
                Err(e) => {
                    eprintln!("warning: failed to check ssh-agent status: {e}");
                    resources.ssh_agent = None;
                    true
                }
            }
        } else {
            true
        };

        if need_start {
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

            while !sock_path.exists() {
                if let Ok(Some(status)) = child.try_wait() {
                    let stderr = child
                        .stderr
                        .take()
                        .and_then(|mut s| {
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

            resources.ssh_agent = Some(SshAgentHandle { child });
        }

        Ok(sock_path)
    }

    pub fn remove_codex_proxy(&self) {
        self.resources.lock().unwrap().codex_proxy = None;
    }

    pub(crate) fn ensure_codex_proxy(
        &self,
        container_url: String,
        container_token: String,
    ) -> Result<CodexProxyEndpoint> {
        {
            let resources = self.resources.lock().unwrap();
            if let Some(handle) = resources.codex_proxy.as_ref() {
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
        let token = crate::daemon::generate_codex_proxy_token();
        tokio::task::spawn(crate::codex::run_codex_proxy(
            tokio_listener,
            container_url,
            container_token,
            token.clone(),
            ready_tx,
            cancel_rx,
        ));
        ready_rx
            .recv()
            .context("waiting for codex proxy accept loop")?;

        self.resources.lock().unwrap().codex_proxy = Some(CodexProxyHandle {
            port,
            token: token.clone(),
            _cancel_tx: cancel_tx,
        });

        Ok(CodexProxyEndpoint { port, token })
    }

    pub fn notify_host_connected(&self) {
        if self.status() == PodConnectionStatus::Stopped {
            return;
        }
        *self.status.lock().unwrap() = PodConnectionStatus::Connecting;
        let _ = self.tx.send(ReconnectEvent::HostConnected);
    }

    pub fn notify_host_disconnected(&self) {
        if self.status() == PodConnectionStatus::Stopped {
            return;
        }
        *self.status.lock().unwrap() = PodConnectionStatus::HostDisconnected;
        let _ = self.tx.send(ReconnectEvent::Attempting);
    }

    pub fn stop_events(&self) {
        self.stop_event_loop();
        *self.status.lock().unwrap() = PodConnectionStatus::Stopped;
        let _ = self.tx.send(ReconnectEvent::Stopped);
    }

    pub fn drop_all(&self) {
        self.stop_events();
        let mut resources = self.resources.lock().unwrap();
        resources.pod_server = None;
        resources.git_tunnel = None;
        resources.forwarded_ports = None;
        resources.ssh_agent = None;
        resources.codex_proxy = None;
        *self.claude_state.lock().unwrap() = None;
        *self.codex_state.lock().unwrap() = None;
    }

    fn stop_event_loop(&self) {
        if let Some(handle) = self.event_loop.lock().unwrap().take() {
            handle.stop.store(true, Ordering::SeqCst);
        }
    }
}

pub struct PodConnectionRegistry {
    host_connections: Arc<HostConnectionRegistry>,
    pods: Mutex<HashMap<PodConnectionKey, Arc<PodConnection>>>,
}

impl PodConnectionRegistry {
    pub fn new(host_connections: Arc<HostConnectionRegistry>) -> Self {
        Self {
            host_connections,
            pods: Mutex::new(HashMap::new()),
        }
    }

    pub fn get_or_create(
        &self,
        repo_path: &Path,
        pod_name: &str,
        host: Host,
        token: String,
    ) -> Result<Arc<PodConnection>> {
        let key = PodConnectionKey::new(repo_path.to_path_buf(), pod_name.to_string());
        let mut pods = self.pods.lock().unwrap();
        if let Some(connection) = pods.get(&key) {
            connection.update_host_and_token(host, token)?;
            return Ok(connection.clone());
        }
        let connection = Arc::new(PodConnection::new(
            self.host_connections.clone(),
            key.clone(),
            host,
            token,
        )?);
        pods.insert(key, connection.clone());
        Ok(connection)
    }

    pub fn get(&self, repo_path: &Path, pod_name: &str) -> Option<Arc<PodConnection>> {
        let key = PodConnectionKey::new(repo_path.to_path_buf(), pod_name.to_string());
        self.pods.lock().unwrap().get(&key).cloned()
    }

    pub fn remove(&self, repo_path: &Path, pod_name: &str) -> Option<Arc<PodConnection>> {
        let key = PodConnectionKey::new(repo_path.to_path_buf(), pod_name.to_string());
        let connection = self.pods.lock().unwrap().remove(&key);
        if let Some(connection) = connection.as_ref() {
            connection.drop_all();
        }
        connection
    }

    pub fn stop_events(&self, repo_path: &Path, pod_name: &str) {
        if let Some(connection) = self.get(repo_path, pod_name) {
            connection.stop_events();
        }
    }

    pub fn endpoint(&self, repo_path: &Path, pod_name: &str) -> Option<PodEndpoint> {
        self.get(repo_path, pod_name)
            .and_then(|connection| connection.endpoint())
    }

    pub fn subscribe(
        &self,
        repo_path: &Path,
        pod_name: &str,
    ) -> Option<broadcast::Receiver<ReconnectEvent>> {
        self.get(repo_path, pod_name).and_then(|connection| {
            if connection.status() == PodConnectionStatus::Stopped || !connection.has_event_loop() {
                None
            } else {
                Some(connection.subscribe())
            }
        })
    }

    pub fn claude_state(&self, repo_path: &Path, pod_name: &str) -> Option<ClaudeState> {
        self.get(repo_path, pod_name)
            .and_then(|connection| connection.claude_state())
    }

    pub fn codex_state(&self, repo_path: &Path, pod_name: &str) -> Option<CodexState> {
        self.get(repo_path, pod_name)
            .and_then(|connection| connection.codex_state())
    }

    pub fn status(&self, repo_path: &Path, pod_name: &str) -> Option<PodConnectionStatus> {
        self.get(repo_path, pod_name)
            .map(|connection| connection.status())
    }

    pub fn notify_host_connected(&self, host: &HostKey) {
        let pods = self.pods.lock().unwrap();
        for connection in pods.values() {
            if &*connection.host_key.lock().unwrap() == host {
                connection.notify_host_connected();
            }
        }
    }

    pub fn notify_host_disconnected(&self, host: &HostKey) {
        let pods = self.pods.lock().unwrap();
        for connection in pods.values() {
            if &*connection.host_key.lock().unwrap() == host {
                connection.notify_host_disconnected();
            }
        }
    }
}

struct GreetingState {
    claude: Option<ClaudeState>,
    codex: Option<CodexState>,
}

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
            let mut data_line = String::new();
            reader.read_line(&mut data_line).ok();
            let mut blank = String::new();
            reader.read_line(&mut blank).ok();

            let greeting = parse_greeting_state(&data_line);
            return Ok((reader, greeting));
        }
    }
}

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

struct PodEventLoop {
    host_conn: Arc<HostConnection>,
    container_url: String,
    token: String,
    stop: Arc<AtomicBool>,
    tx: broadcast::Sender<ReconnectEvent>,
    status: Arc<Mutex<PodConnectionStatus>>,
    claude_state: Arc<Mutex<Option<ClaudeState>>>,
    codex_state: Arc<Mutex<Option<CodexState>>>,
}

fn pod_event_loop(ctx: PodEventLoop) {
    let PodEventLoop {
        host_conn,
        container_url,
        token,
        stop,
        tx,
        status,
        claude_state,
        codex_state,
    } = ctx;

    let apply_greeting = |g: GreetingState| {
        *claude_state.lock().unwrap() = g.claude;
        *codex_state.lock().unwrap() = g.codex;
    };

    let mut host_rx = host_conn.subscribe();
    let mut backoff = INITIAL_DELAY;

    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }

        let _ = tx.send(ReconnectEvent::Attempting);
        host_conn.request_probe();
        if !wait_for_host(&mut host_rx, &stop, &status) {
            return;
        }
        *status.lock().unwrap() = PodConnectionStatus::Connecting;
        let _ = tx.send(ReconnectEvent::HostConnected);

        let mut reader = match connect_pod_events(&container_url, &token) {
            Ok((reader, greeting)) => {
                apply_greeting(greeting);
                *status.lock().unwrap() = PodConnectionStatus::Connected;
                let _ = tx.send(ReconnectEvent::Connected);
                backoff = INITIAL_DELAY;
                reader
            }
            Err(e) => {
                debug!("pod event connection failed: {e:#}");
                *status.lock().unwrap() = PodConnectionStatus::PodDisconnected;
                let _ = tx.send(ReconnectEvent::Failed {
                    error: format!("{e:#}"),
                });
                host_conn.request_probe();
                std::thread::sleep(jitter(backoff));
                backoff = std::cmp::min(backoff.saturating_mul(2), MAX_DELAY);
                continue;
            }
        };

        let mut pending_event: Option<String> = None;
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if host_conn.status() == HostStatus::Disconnected {
                *status.lock().unwrap() = PodConnectionStatus::HostDisconnected;
                let _ = tx.send(ReconnectEvent::Attempting);
                break;
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
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
                            Some("state") | None => {}
                            Some(_) => {}
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

fn wait_for_host(
    host_rx: &mut watch::Receiver<HostStatus>,
    stop: &AtomicBool,
    status: &Mutex<PodConnectionStatus>,
) -> bool {
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        if *host_rx.borrow() == HostStatus::Connected {
            return true;
        }
        *status.lock().unwrap() = PodConnectionStatus::HostDisconnected;
        let _ = RUNTIME.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), host_rx.changed()).await
        });
    }
}
