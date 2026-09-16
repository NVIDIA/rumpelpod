// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-pod daemon connection state.
//!
//! A `PodConnection` owns the host-side resources that make a running
//! pod reachable from the daemon: the pod-server exec proxy, the git
//! tunnel, user-facing forwarded ports, its managed ssh-agent, and the
//! codex proxy. It does not hold a `HostConnection`; loops that need
//! both live on `Connections`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{error, info};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::broadcast;

use crate::async_command::AsyncCommandExt;

use crate::config::Host;
use crate::daemon::host_connection::HostKey;
use crate::daemon::protocol::{DaemonEvent, PodName};
use crate::daemon::reconnect::ReconnectEvent;
use crate::daemon::{CodexProxyEndpoint, CodexProxyHandle};
use crate::pod::client::PodClient;
use crate::pod::types::{ClaudeState, CodexState};

const POD_REPAIR_INITIAL_DELAY: Duration = Duration::from_secs(1);
const POD_REPAIR_MAX_DELAY: Duration = Duration::from_secs(30);
const POD_REPAIR_FAILURE_LIMIT: usize = 10;

const SSH_AGENT_START_TIMEOUT: Duration = Duration::from_secs(5);

struct ManagedSshAgent {
    child: Child,
    agent_dir: PathBuf,
    socket_path: PathBuf,
    configured_keys_hash: Option<String>,
}

impl ManagedSshAgent {
    async fn configured(key: &PodConnectionKey, keys: &[PathBuf]) -> Result<Self> {
        let configured_keys_hash = Self::configured_keys_hash(keys)?;
        let mut agent = Self::start(key).await?;
        agent.add_configured_keys(keys).await?;
        agent.configured_keys_hash = Some(configured_keys_hash);
        Ok(agent)
    }

    fn configured_keys_hash(keys: &[PathBuf]) -> Result<String> {
        let mut hasher = Sha256::new();
        for key in keys {
            hasher.update(key.as_os_str().as_encoded_bytes());
            hasher.update([0]);
            hasher.update(std::fs::read(key).with_context(|| {
                let key = key.display();
                format!("reading configured SSH key {key}")
            })?);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    async fn start(key: &PodConnectionKey) -> Result<Self> {
        let pod_name = PodName(key.pod_name.clone());
        let agent_dir = crate::daemon::ssh_agent_dir(&key.repo_path, &pod_name);
        match std::fs::remove_dir_all(&agent_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let agent_dir = agent_dir.display();
                return Err(error)
                    .with_context(|| format!("removing stale ssh-agent directory {agent_dir}"));
            }
        }
        std::fs::create_dir_all(&agent_dir).with_context(|| {
            let agent_dir = agent_dir.display();
            format!("creating ssh-agent directory {agent_dir}")
        })?;

        let socket_path = agent_dir.join("agent.sock");
        let child = match TokioCommand::new("ssh-agent")
            .kill_on_drop(true)
            .args(["-D", "-a"])
            .arg(&socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                if let Err(cleanup_error) = std::fs::remove_dir_all(&agent_dir) {
                    let agent_dir = agent_dir.display();
                    error!(
                        "failed to remove ssh-agent directory {agent_dir} after startup failure: {cleanup_error}"
                    );
                }
                return Err(error).context("failed to start ssh-agent");
            }
        };
        let mut agent = Self {
            child,
            agent_dir,
            socket_path,
            configured_keys_hash: None,
        };
        let deadline = Instant::now() + SSH_AGENT_START_TIMEOUT;
        while !agent.socket_path.exists() {
            match agent.child.try_wait() {
                Ok(Some(status)) => {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = agent.child.stderr.take() {
                        pipe.read_to_string(&mut stderr)
                            .await
                            .context("reading ssh-agent error")?;
                    }
                    let stderr = stderr.trim();
                    return Err(anyhow::anyhow!("ssh-agent exited with {status}: {stderr}"));
                }
                Ok(None) => {}
                Err(error) => return Err(error).context("checking ssh-agent startup"),
            }
            if Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "ssh-agent did not create its socket within {SSH_AGENT_START_TIMEOUT:?}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(agent)
    }

    fn is_alive(&mut self) -> Result<bool> {
        match self.child.try_wait() {
            Ok(Some(_)) => Ok(false),
            Ok(None) => Ok(true),
            Err(error) => Err(error).context("checking ssh-agent status"),
        }
    }

    async fn add_configured_keys(&mut self, keys: &[PathBuf]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let output = Command::new("ssh-add")
            .args(keys)
            .env("SSH_AUTH_SOCK", &self.socket_path)
            .env("SSH_ASKPASS_REQUIRE", "never")
            .stdin(Stdio::null())
            .output_async()
            .await
            .context("running ssh-add for configured SSH keys")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            return Err(anyhow::anyhow!(
                "ssh-add failed while loading configured SSH keys with status {}: {stderr}",
                output.status
            ));
        }
        Ok(())
    }
}

impl Drop for ManagedSshAgent {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = self.child.start_kill() {
                    error!("failed to kill ssh-agent: {error}");
                }
            }
            Err(error) => error!("failed to check ssh-agent before cleanup: {error}"),
        }
        match std::fs::remove_dir_all(&self.agent_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let agent_dir = self.agent_dir.display();
                error!("failed to remove ssh-agent directory {agent_dir}: {error}");
            }
        }
    }
}

pub(super) struct PodRepairBackoff {
    pub(super) failures: usize,
    delay: Duration,
}

impl PodRepairBackoff {
    pub(super) fn new() -> Self {
        Self {
            failures: 0,
            delay: POD_REPAIR_INITIAL_DELAY,
        }
    }

    pub(super) fn reset(&mut self) {
        self.failures = 0;
        self.delay = POD_REPAIR_INITIAL_DELAY;
    }

    pub(super) fn next_delay(&mut self) -> Option<Duration> {
        self.failures += 1;
        if self.failures >= POD_REPAIR_FAILURE_LIMIT {
            return None;
        }
        let delay = crate::jitter(self.delay);
        self.delay = std::cmp::min(self.delay.saturating_mul(2), POD_REPAIR_MAX_DELAY);
        Some(delay)
    }
}

#[derive(Default)]
struct PodRepairState {
    paused: AtomicBool,
    epoch: Mutex<u64>,
    wake: Condvar,
}

impl PodRepairState {
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.signal();
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.signal();
    }

    fn signal(&self) {
        let mut epoch = self.epoch.lock().unwrap();
        *epoch = epoch.wrapping_add(1);
        drop(epoch);
        self.wake.notify_all();
    }

    fn epoch(&self) -> u64 {
        *self.epoch.lock().unwrap()
    }

    fn wait_for_change(&self, observed_epoch: u64, delay: Duration) -> u64 {
        let epoch = self.epoch.lock().unwrap();
        if *epoch != observed_epoch {
            return *epoch;
        }
        let (epoch, _) = self.wake.wait_timeout(epoch, delay).unwrap();
        *epoch
    }
}

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

pub(super) fn replace_and_emit<T>(
    value: &Mutex<T>,
    replacement: T,
    events_tx: &broadcast::Sender<DaemonEvent>,
    key: &PodConnectionKey,
) where
    T: PartialEq,
{
    let changed = {
        let mut current = value.lock().unwrap();
        if *current == replacement {
            false
        } else {
            *current = replacement;
            true
        }
    };
    if changed {
        let _ = events_tx.send(DaemonEvent::PodStatusChanged {
            repository: key.repo_path.to_string_lossy().into_owned(),
            pod: key.pod_name.clone(),
        });
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
    supervise_git_tunnel: bool,
    validate_pod_before_git_tunnel_repair: bool,
    forwarded_ports: Option<Vec<crate::exec_proxy::ExecProxyHandle>>,
    codex_proxy: Option<CodexProxyHandle>,
}

impl PodConnectionResources {
    fn new() -> Self {
        Self {
            pod_server: None,
            git_tunnel: None,
            supervise_git_tunnel: false,
            validate_pod_before_git_tunnel_repair: false,
            forwarded_ports: None,
            codex_proxy: None,
        }
    }
}

struct EventLoopHandle {
    stop: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
}

pub struct PodConnection {
    key: PodConnectionKey,
    host: Mutex<Host>,
    host_key: Mutex<HostKey>,
    token: Mutex<String>,
    status: Arc<Mutex<PodConnectionStatus>>,
    tx: broadcast::Sender<ReconnectEvent>,
    claude_state: Arc<Mutex<Option<ClaudeState>>>,
    codex_state: Arc<Mutex<Option<CodexState>>>,
    resources: Mutex<PodConnectionResources>,
    /// Reconnect and recreation preserve the connection, so the agent stays here.
    managed_ssh_agent: Mutex<Option<ManagedSshAgent>>,
    ssh_agent_setup: tokio::sync::Mutex<()>,
    git_tunnel_setup: tokio::sync::Mutex<()>,
    git_tunnel_repair_scheduled: AtomicBool,
    repair_state: Arc<PodRepairState>,
    event_loop: Mutex<Option<EventLoopHandle>>,
    events_tx: broadcast::Sender<DaemonEvent>,
}

impl PodConnection {
    fn new(
        events_tx: broadcast::Sender<DaemonEvent>,
        key: PodConnectionKey,
        host: Host,
        token: String,
        status: PodConnectionStatus,
        managed_ssh_agent: Option<ManagedSshAgent>,
    ) -> Self {
        let host_key = HostKey::from_host(&host);
        let (tx, _) = broadcast::channel(64);
        Self {
            key,
            host: Mutex::new(host),
            host_key: Mutex::new(host_key),
            token: Mutex::new(token),
            status: Arc::new(Mutex::new(status)),
            tx,
            claude_state: Arc::new(Mutex::new(None)),
            codex_state: Arc::new(Mutex::new(None)),
            resources: Mutex::new(PodConnectionResources::new()),
            managed_ssh_agent: Mutex::new(managed_ssh_agent),
            ssh_agent_setup: tokio::sync::Mutex::new(()),
            git_tunnel_setup: tokio::sync::Mutex::new(()),
            git_tunnel_repair_scheduled: AtomicBool::new(false),
            repair_state: Arc::new(PodRepairState::default()),
            event_loop: Mutex::new(None),
            events_tx,
        }
    }

    pub fn key(&self) -> &PodConnectionKey {
        &self.key
    }

    pub fn update_host_and_token(&self, host: Host, token: String) {
        let host_key = HostKey::from_host(&host);
        let should_update = *self.host_key.lock().unwrap() != host_key;
        if should_update {
            *self.host.lock().unwrap() = host;
            *self.host_key.lock().unwrap() = host_key;
            self.stop_event_loop();
        }
        *self.token.lock().unwrap() = token;
    }

    pub fn ensure_ssh_agent(&self) -> Result<PathBuf> {
        crate::async_runtime::block_on(self.ensure_ssh_agent_async())
    }

    async fn ensure_ssh_agent_async(&self) -> Result<PathBuf> {
        let _setup = self.ssh_agent_setup.lock().await;
        {
            let mut slot = self.managed_ssh_agent.lock().unwrap();
            if let Some(agent) = slot.as_mut() {
                if agent.is_alive()? {
                    return Ok(agent.socket_path.clone());
                }
            }
            slot.take();
        }
        let agent = ManagedSshAgent::start(&self.key).await?;
        let socket_path = agent.socket_path.clone();
        *self.managed_ssh_agent.lock().unwrap() = Some(agent);
        Ok(socket_path)
    }

    async fn configure_ssh_agent(&self, keys: &[PathBuf]) -> Result<PathBuf> {
        let configured_keys_hash = ManagedSshAgent::configured_keys_hash(keys)?;
        let _setup = self.ssh_agent_setup.lock().await;
        {
            let mut slot = self.managed_ssh_agent.lock().unwrap();
            if let Some(agent) = slot.as_mut() {
                if agent.configured_keys_hash.as_ref() == Some(&configured_keys_hash)
                    && agent.is_alive()?
                {
                    return Ok(agent.socket_path.clone());
                }
            }
            slot.take();
        }
        let agent = ManagedSshAgent::configured(&self.key, keys).await?;
        let socket_path = agent.socket_path.clone();
        *self.managed_ssh_agent.lock().unwrap() = Some(agent);
        Ok(socket_path)
    }

    pub fn remove_ssh_agent(&self) {
        self.managed_ssh_agent.lock().unwrap().take();
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

    pub fn probe(&self) -> Result<()> {
        crate::async_runtime::block_on(self.probe_async())
    }

    pub async fn probe_async(&self) -> Result<()> {
        let endpoint = self
            .endpoint()
            .context("pod connection has no live endpoint")?;
        PodClient::new_with_timeout(&endpoint.url, &endpoint.token, Duration::from_secs(5))?
            .get_state_async()
            .await
            .context("probing pod connection")?;
        Ok(())
    }

    pub fn has_alive_pod_server(&self) -> bool {
        self.resources
            .lock()
            .unwrap()
            .pod_server
            .as_ref()
            .is_some_and(|h| h.is_alive())
    }

    pub fn pod_server_targets_different_container(&self, container: &str) -> bool {
        self.resources
            .lock()
            .unwrap()
            .pod_server
            .as_ref()
            .is_some_and(|handle| handle.target_container() != container)
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
        self.update_host_and_token(host, token);
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

    /// Reserve the event-loop slot. Returns the stop flag for the
    /// thread `Connections` will spawn.
    pub(super) fn begin_event_loop(&self) -> Option<Arc<AtomicBool>> {
        let mut event_loop = self.event_loop.lock().unwrap();
        if event_loop.is_some() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        *event_loop = Some(EventLoopHandle {
            stop: stop.clone(),
            _thread: None,
        });
        Some(stop)
    }

    pub(super) fn finish_event_loop(&self, thread: JoinHandle<()>) {
        if let Some(handle) = self.event_loop.lock().unwrap().as_mut() {
            handle._thread = Some(thread);
        }
    }

    pub fn git_tunnel_is_alive(&self) -> bool {
        self.resources
            .lock()
            .unwrap()
            .git_tunnel
            .as_ref()
            .is_some_and(|h| h.is_alive())
    }

    pub fn git_tunnel_targets_container(&self, container: &str) -> bool {
        self.resources
            .lock()
            .unwrap()
            .git_tunnel
            .as_ref()
            .is_some_and(|handle| handle.target_container() == container)
    }

    pub fn remove_git_tunnel(&self) {
        self.resources.lock().unwrap().git_tunnel = None;
    }

    pub fn set_git_tunnel(&self, handle: crate::tunnel::TunnelHandle) {
        let mut resources = self.resources.lock().unwrap();
        resources.git_tunnel = Some(handle);
        resources.supervise_git_tunnel = true;
        resources.validate_pod_before_git_tunnel_repair = false;
        drop(resources);
        self.repair_state.resume();
    }

    pub fn enable_git_tunnel_supervision(&self) {
        let mut resources = self.resources.lock().unwrap();
        resources.supervise_git_tunnel = true;
        resources.validate_pod_before_git_tunnel_repair = resources.git_tunnel.is_none();
        drop(resources);
        self.repair_state.resume();
    }

    pub fn git_tunnel_supervision_enabled(&self) -> bool {
        self.resources.lock().unwrap().supervise_git_tunnel
    }

    pub fn validate_pod_before_git_tunnel_repair(&self) -> bool {
        self.resources
            .lock()
            .unwrap()
            .validate_pod_before_git_tunnel_repair
    }

    pub fn mark_pod_validated_for_git_tunnel_repair(&self) {
        self.resources
            .lock()
            .unwrap()
            .validate_pod_before_git_tunnel_repair = false;
    }

    pub fn disable_git_tunnel_supervision(&self) {
        let mut resources = self.resources.lock().unwrap();
        resources.supervise_git_tunnel = false;
        resources.validate_pod_before_git_tunnel_repair = false;
        resources.git_tunnel = None;
    }

    pub fn git_tunnel_setup_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        crate::async_runtime::block_on(self.git_tunnel_setup_guard_async())
    }

    pub async fn git_tunnel_setup_guard_async(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.git_tunnel_setup.lock().await
    }

    pub fn try_schedule_git_tunnel_repair(&self) -> bool {
        if self.repair_state.is_paused() {
            return false;
        }
        let resources = self.resources.lock().unwrap();
        if !resources.supervise_git_tunnel
            || resources
                .git_tunnel
                .as_ref()
                .is_some_and(|handle| handle.is_alive())
        {
            return false;
        }
        drop(resources);

        self.git_tunnel_repair_scheduled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn finish_git_tunnel_repair(&self) {
        self.git_tunnel_repair_scheduled
            .store(false, Ordering::SeqCst);
    }

    pub fn pod_repair_is_paused(&self) -> bool {
        self.repair_state.is_paused()
    }

    pub fn pause_pod_repair(&self) {
        self.repair_state.pause();
    }

    pub fn pod_repair_epoch(&self) -> u64 {
        self.repair_state.epoch()
    }

    pub fn wait_for_pod_repair_change(&self, epoch: u64, delay: Duration) -> u64 {
        self.repair_state.wait_for_change(epoch, delay)
    }

    pub fn install_repaired_git_tunnel(
        &self,
        handle: crate::tunnel::TunnelHandle,
        repair_epoch: u64,
    ) {
        let mut resources = self.resources.lock().unwrap();
        // Host transitions cannot wait for a stalled repair. Reject its late
        // result instead so it cannot overwrite the transition's cleanup.
        if resources.supervise_git_tunnel && self.repair_state.epoch() == repair_epoch {
            resources.git_tunnel = Some(handle);
            resources.validate_pod_before_git_tunnel_repair = false;
        }
    }

    pub fn host(&self) -> Host {
        self.host.lock().unwrap().clone()
    }

    pub(super) fn set_status(&self, status: PodConnectionStatus) {
        replace_and_emit(&self.status, status, &self.events_tx, &self.key);
    }

    pub(super) fn emit_reconnect(&self, event: ReconnectEvent) {
        let _ = self.tx.send(event);
    }

    pub(super) fn apply_greeting(&self, greeting: GreetingState) {
        self.set_claude_state(greeting.claude);
        self.set_codex_state(greeting.codex);
    }

    pub(super) fn set_claude_state(&self, state: Option<ClaudeState>) {
        replace_and_emit(&self.claude_state, state, &self.events_tx, &self.key);
    }

    pub(super) fn set_codex_state(&self, state: Option<CodexState>) {
        replace_and_emit(&self.codex_state, state, &self.events_tx, &self.key);
    }

    pub(super) fn resume_repair(&self) {
        self.repair_state.resume();
    }

    pub fn has_forwarded_ports(&self) -> bool {
        self.resources.lock().unwrap().forwarded_ports.is_some()
    }

    pub fn forwarded_ports_target_containers_match(&self, expected: &[String]) -> bool {
        let resources = self.resources.lock().unwrap();
        let Some(handles) = resources.forwarded_ports.as_ref() else {
            return false;
        };
        let mut actual: Vec<&str> = handles
            .iter()
            .map(crate::exec_proxy::ExecProxyHandle::target_container)
            .collect();
        let mut expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        actual.sort_unstable();
        expected.sort_unstable();
        actual == expected
    }

    pub fn remove_forwarded_ports(&self) {
        self.resources.lock().unwrap().forwarded_ports = None;
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

    pub fn remove_codex_proxy(&self) {
        self.resources.lock().unwrap().codex_proxy = None;
    }

    pub(crate) fn ensure_codex_proxy(
        &self,
        container_url: String,
        container_token: String,
        bypass: bool,
    ) -> Result<CodexProxyEndpoint> {
        {
            let resources = self.resources.lock().unwrap();
            if let Some(handle) = resources.codex_proxy.as_ref() {
                // A replacement frontend may choose different startup defaults.
                // Reconnects through this cached proxy must carry that choice.
                handle.bypass.store(bypass, Ordering::Relaxed);
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
        let bypass = Arc::new(AtomicBool::new(bypass));
        tokio::task::spawn(crate::codex::run_codex_proxy(
            tokio_listener,
            container_url,
            container_token,
            token.clone(),
            Arc::clone(&bypass),
            ready_tx,
            cancel_rx,
        ));
        ready_rx
            .recv()
            .context("waiting for codex proxy accept loop")?;

        self.resources.lock().unwrap().codex_proxy = Some(CodexProxyHandle {
            port,
            token: token.clone(),
            bypass,
            _cancel_tx: cancel_tx,
        });

        Ok(CodexProxyEndpoint { port, token })
    }

    pub fn notify_host_connected(&self) {
        if self.status() == PodConnectionStatus::Stopped {
            return;
        }
        self.repair_state.resume();
        info!(
            "pod connection for '{}' is reconnecting after host recovery",
            self.key.pod_name
        );
        self.set_status(PodConnectionStatus::Connecting);
        self.emit_reconnect(ReconnectEvent::HostConnected);
    }

    pub fn notify_host_disconnected(&self) {
        if self.status() == PodConnectionStatus::Stopped {
            return;
        }
        // A repair may be queued behind another pod's SSH probe. The host
        // transition must invalidate every pod without waiting for that work.
        self.repair_state.signal();
        info!(
            "pod connection for '{}' lost its host; resetting active transports",
            self.key.pod_name
        );
        self.set_status(PodConnectionStatus::HostDisconnected);
        let mut resources = self.resources.lock().unwrap();
        if resources.supervise_git_tunnel {
            resources.git_tunnel = None;
            resources.validate_pod_before_git_tunnel_repair = true;
        }
        drop(resources);
        self.emit_reconnect(ReconnectEvent::Attempting);
    }

    pub fn stop_events(&self) {
        self.stop_event_loop();
        self.set_status(PodConnectionStatus::Stopped);
        self.disable_git_tunnel_supervision();
        self.emit_reconnect(ReconnectEvent::Stopped);
    }

    pub fn reset_for_recreate(&self) {
        self.stop_events();
        let mut resources = self.resources.lock().unwrap();
        resources.pod_server = None;
        resources.git_tunnel = None;
        resources.forwarded_ports = None;
        resources.codex_proxy = None;
        *self.claude_state.lock().unwrap() = None;
        *self.codex_state.lock().unwrap() = None;
    }

    pub fn drop_all(&self) {
        self.reset_for_recreate();
        self.remove_ssh_agent();
    }

    fn stop_event_loop(&self) {
        if let Some(handle) = self.event_loop.lock().unwrap().take() {
            handle.stop.store(true, Ordering::SeqCst);
            self.repair_state.signal();
        }
    }
}

pub struct PodConnectionRegistry {
    pods: Mutex<HashMap<PodConnectionKey, Arc<PodConnection>>>,
    events_tx: broadcast::Sender<DaemonEvent>,
}

impl PodConnectionRegistry {
    pub fn new(events_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self {
            pods: Mutex::new(HashMap::new()),
            events_tx,
        }
    }

    pub fn get_or_create(
        &self,
        repo_path: &Path,
        pod_name: &str,
        host: Host,
        token: String,
        initial_status: PodConnectionStatus,
        configured_ssh_keys: Option<&[PathBuf]>,
    ) -> Result<Arc<PodConnection>> {
        crate::async_runtime::block_on(self.get_or_create_async(
            repo_path,
            pod_name,
            host,
            token,
            initial_status,
            configured_ssh_keys,
        ))
    }

    pub async fn get_or_create_async(
        &self,
        repo_path: &Path,
        pod_name: &str,
        host: Host,
        token: String,
        initial_status: PodConnectionStatus,
        configured_ssh_keys: Option<&[PathBuf]>,
    ) -> Result<Arc<PodConnection>> {
        let key = PodConnectionKey::new(repo_path.to_path_buf(), pod_name.to_string());
        let connection = {
            let mut pods = self.pods.lock().unwrap();
            pods.entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(PodConnection::new(
                        self.events_tx.clone(),
                        key,
                        host.clone(),
                        token.clone(),
                        initial_status,
                        None,
                    ))
                })
                .clone()
        };
        connection.update_host_and_token(host, token);
        match configured_ssh_keys {
            Some(keys) => {
                connection.configure_ssh_agent(keys).await?;
            }
            None => connection.remove_ssh_agent(),
        }
        Ok(connection)
    }

    pub fn all(&self) -> Vec<Arc<PodConnection>> {
        self.pods.lock().unwrap().values().cloned().collect()
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

pub(super) struct GreetingState {
    pub(super) claude: Option<ClaudeState>,
    pub(super) codex: Option<CodexState>,
}

pub(super) fn connect_pod_events(
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

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;

    use super::*;
    use crate::config::ContainerEngine;

    #[test]
    fn connection_and_agent_state_changes_emit_status_invalidations() {
        let (events_tx, mut events_rx) = broadcast::channel(4);
        let key = PodConnectionKey::new("/repo", "test");
        let status = Mutex::new(PodConnectionStatus::Connecting);

        replace_and_emit(&status, PodConnectionStatus::Connecting, &events_tx, &key);
        assert!(matches!(
            events_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        replace_and_emit(&status, PodConnectionStatus::Connected, &events_tx, &key);
        let expected = DaemonEvent::PodStatusChanged {
            repository: "/repo".to_string(),
            pod: "test".to_string(),
        };
        assert_eq!(events_rx.try_recv(), Ok(expected.clone()));

        let claude_state = Mutex::new(None);
        replace_and_emit(
            &claude_state,
            Some(ClaudeState::Processing),
            &events_tx,
            &key,
        );
        assert_eq!(events_rx.try_recv(), Ok(expected));
    }

    #[test]
    fn pod_repair_backoff_eventually_pauses() {
        let mut backoff = PodRepairBackoff::new();

        for _ in 1..POD_REPAIR_FAILURE_LIMIT {
            assert!(backoff.next_delay().is_some());
        }
        assert!(backoff.next_delay().is_none());

        backoff.reset();
        assert!(backoff.next_delay().is_some());
    }

    #[test]
    fn removing_pod_connection_discards_cached_codex_state() {
        let (daemon_events_tx, _daemon_events_rx) = broadcast::channel(1);
        let registry = PodConnectionRegistry::new(daemon_events_tx);
        let repo_path = Path::new("/repo");
        let host = Host::Localhost {
            engine: ContainerEngine::Docker,
        };

        let old = registry
            .get_or_create(
                repo_path,
                "test",
                host.clone(),
                "old-token".to_string(),
                PodConnectionStatus::HostDisconnected,
                None,
            )
            .expect("create old pod connection");
        *old.codex_state.lock().unwrap() = Some(CodexState::Idle);

        registry.remove(repo_path, "test").unwrap();
        let replacement = registry
            .get_or_create(
                repo_path,
                "test",
                host,
                "new-token".to_string(),
                PodConnectionStatus::HostDisconnected,
                None,
            )
            .expect("create replacement pod connection");

        assert!(!Arc::ptr_eq(&old, &replacement));
        assert_eq!(replacement.codex_state(), None);
    }
}
