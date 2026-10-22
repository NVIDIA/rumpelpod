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
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use log::info;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, Notify};
use tokio_util::io::StreamReader;

use crate::config::Host;
use crate::daemon::host_connection::HostKey;
use crate::daemon::protocol::{DaemonEvent, PodName};
use crate::daemon::reconnect::ReconnectEvent;
use crate::daemon::{CodexProxyEndpoint, CodexProxyHandle};
use crate::pod::client::PodClient;
use crate::pod::types::{ClaudeState, CodexState};
use crate::ssh_agent::ManagedSshAgent;

const POD_REPAIR_INITIAL_DELAY: Duration = Duration::from_secs(1);
const POD_REPAIR_MAX_DELAY: Duration = Duration::from_secs(30);
const POD_REPAIR_FAILURE_LIMIT: usize = 10;

fn claude_is_processing(state: Option<ClaudeState>) -> bool {
    match state {
        Some(ClaudeState::Processing) => true,
        Some(ClaudeState::WaitingForInput | ClaudeState::AuthError | ClaudeState::Stopped)
        | None => false,
    }
}

fn codex_is_processing(state: Option<CodexState>) -> bool {
    match state {
        Some(CodexState::Processing) => true,
        Some(CodexState::Idle | CodexState::Error) | None => false,
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
    async_wake: Notify,
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
        self.async_wake.notify_waiters();
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

type ActivityObserver = Arc<dyn Fn(&PodConnectionKey, bool) + Send + Sync>;

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
    connection_requested: AtomicBool,
    last_agent_processing: AtomicBool,
    activity_observer: Mutex<Option<ActivityObserver>>,
    // Lock before changing the event epoch, publishing a greeting, or retiring
    // demand, so a late observation cannot revive an invalidated connection.
    activity_update: Mutex<()>,
    repair_state: Arc<PodRepairState>,
    event_loop: Mutex<Option<EventLoopHandle>>,
    event_http_version: Mutex<Option<reqwest::Version>>,
    events_tx: broadcast::Sender<DaemonEvent>,
    /// The event stream keeps a transport open so executions avoid another
    /// remote handshake, including across concurrent CLI processes.
    pub(crate) http_client: reqwest::Client,
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
            connection_requested: AtomicBool::new(false),
            last_agent_processing: AtomicBool::new(false),
            activity_observer: Mutex::new(None),
            activity_update: Mutex::new(()),
            repair_state: Arc::new(PodRepairState::default()),
            event_loop: Mutex::new(None),
            event_http_version: Mutex::new(None),
            events_tx,
            http_client: crate::exec_stream::client_builder()
                .build()
                .expect("building pod HTTP/2 client"),
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
        let agent = ManagedSshAgent::start(crate::daemon::ssh_agent_dir(
            &self.key.repo_path,
            &PodName(self.key.pod_name.clone()),
        ))
        .await?;
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
        let agent = ManagedSshAgent::configured(
            crate::daemon::ssh_agent_dir(&self.key.repo_path, &PodName(self.key.pod_name.clone())),
            keys,
        )
        .await?;
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
        let _guard = self.activity_update.lock().unwrap();
        let rx = self.tx.subscribe();
        if self.repair_state.is_paused() || self.status() != PodConnectionStatus::Connected {
            self.repair_state.resume();
        }
        if self.status() == PodConnectionStatus::Connected {
            self.finish_connection_request_locked();
        }
        rx
    }

    pub fn agent_is_processing(&self) -> bool {
        self.last_agent_processing.load(Ordering::SeqCst)
    }

    pub(super) fn restore_agent_processing(&self) {
        let _update = self.activity_update.lock().unwrap();
        self.last_agent_processing.store(true, Ordering::SeqCst);
    }

    pub(super) fn set_activity_observer(&self, observer: ActivityObserver) {
        let _update = self.activity_update.lock().unwrap();
        *self.activity_observer.lock().unwrap() = Some(observer);
    }

    fn update_agent_activity(&self, epoch: Option<u64>, update: impl FnOnce()) -> bool {
        let _guard = self.activity_update.lock().unwrap();
        self.update_agent_activity_locked(epoch, update)
    }

    fn update_agent_activity_locked(&self, epoch: Option<u64>, update: impl FnOnce()) -> bool {
        if epoch.is_some_and(|epoch| epoch != self.pod_repair_epoch()) {
            return false;
        }
        update();
        let processing =
            claude_is_processing(self.claude_state()) || codex_is_processing(self.codex_state());
        let previous = self
            .last_agent_processing
            .swap(processing, Ordering::SeqCst);
        if previous != processing {
            let observer = self.activity_observer.lock().unwrap().clone();
            if let Some(observer) = observer {
                observer(&self.key, processing);
            }
        }
        let _ = self.events_tx.send(DaemonEvent::PodStatusChanged {
            repository: self.key.repo_path.to_string_lossy().into_owned(),
            pod: self.key.pod_name.clone(),
        });
        true
    }

    pub fn wants_reconnect(&self) -> bool {
        self.connection_requested.load(Ordering::SeqCst)
            || self.tx.receiver_count() > 0
            || self.agent_is_processing()
    }

    pub fn request_connection(&self) {
        let _guard = self.activity_update.lock().unwrap();
        // Foreground preparation returns before its event observer is ready.
        // Keep this one request alive until both transports finish connecting.
        self.connection_requested.store(true, Ordering::SeqCst);
        self.finish_connection_request_locked();
        if self.connection_requested.load(Ordering::SeqCst)
            && (self.repair_state.is_paused() || self.status() != PodConnectionStatus::Connected)
        {
            self.repair_state.resume();
        }
    }

    pub(super) fn finish_connection_request(&self) {
        let _guard = self.activity_update.lock().unwrap();
        self.finish_connection_request_locked();
    }

    fn finish_connection_request_locked(&self) {
        if self.status() == PodConnectionStatus::Connected {
            let resources = self.resources.lock().unwrap();
            if !resources.supervise_git_tunnel
                || resources
                    .git_tunnel
                    .as_ref()
                    .is_some_and(|tunnel| tunnel.is_alive())
            {
                self.connection_requested.store(false, Ordering::SeqCst);
                self.emit_reconnect(ReconnectEvent::Connected);
            }
        }
    }

    pub(crate) fn event_http_version(&self) -> Result<reqwest::Version> {
        (*self.event_http_version.lock().unwrap())
            .context("pod event connection has not established its HTTP version")
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
        self.exec_endpoint().map(|(endpoint, _)| endpoint)
    }

    pub fn exec_endpoint(&self) -> Option<(PodEndpoint, String)> {
        let resources = self.resources.lock().unwrap();
        let handle = resources.pod_server.as_ref()?;
        if !handle.is_alive() {
            return None;
        }
        Some((
            PodEndpoint {
                url: format!("http://127.0.0.1:{}", handle.port),
                token: self.token.lock().unwrap().clone(),
            },
            handle.target_container().to_string(),
        ))
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
        self.begin_event_loop_at_epoch(None)
    }

    pub(super) fn begin_event_loop_at_epoch(&self, epoch: Option<u64>) -> Option<Arc<AtomicBool>> {
        let mut event_loop = self.event_loop.lock().unwrap();
        if event_loop.is_some() || epoch.is_some_and(|epoch| epoch != self.pod_repair_epoch()) {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        *event_loop = Some(EventLoopHandle {
            stop: stop.clone(),
            _thread: None,
        });
        self.set_status(PodConnectionStatus::Connecting);
        Some(stop)
    }

    pub(super) fn install_repaired_routes(
        &self,
        proxy: crate::exec_proxy::ExecProxyHandle,
        ports: Option<Vec<crate::exec_proxy::ExecProxyHandle>>,
        epoch: u64,
    ) -> bool {
        let mut resources = self.resources.lock().unwrap();
        if !resources.supervise_git_tunnel
            || self.pod_repair_epoch() != epoch
            || resources
                .pod_server
                .as_ref()
                .is_some_and(|proxy| proxy.is_alive())
        {
            return false;
        }
        // Publish a complete set without changing identity. Stop/recreation
        // invalidates the epoch and disposes of any superseded resources.
        resources.pod_server = Some(proxy);
        if let Some(ports) = ports {
            resources.forwarded_ports = Some(ports);
        }
        true
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
        let _guard = self.activity_update.lock().unwrap();
        let mut resources = self.resources.lock().unwrap();
        resources.git_tunnel = Some(handle);
        resources.supervise_git_tunnel = true;
        resources.validate_pod_before_git_tunnel_repair = false;
        drop(resources);
        if self.repair_state.is_paused() {
            self.repair_state.resume();
        }
        self.finish_connection_request_locked();
    }

    pub fn enable_git_tunnel_supervision(&self) {
        let _guard = self.activity_update.lock().unwrap();
        let mut resources = self.resources.lock().unwrap();
        resources.supervise_git_tunnel = true;
        resources.validate_pod_before_git_tunnel_repair = resources.git_tunnel.is_none();
        drop(resources);
        if self.repair_state.is_paused() {
            self.repair_state.resume();
        }
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

    pub fn try_git_tunnel_setup_guard(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        self.git_tunnel_setup.try_lock().ok()
    }

    pub async fn git_tunnel_setup_guard_async(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.git_tunnel_setup.lock().await
    }

    pub fn try_schedule_git_tunnel_repair(&self) -> bool {
        if self.repair_state.is_paused() || !self.wants_reconnect() {
            return false;
        }
        let resources = self.resources.lock().unwrap();
        if !resources.supervise_git_tunnel
            || (resources
                .git_tunnel
                .as_ref()
                .is_some_and(|handle| handle.is_alive())
                && resources
                    .pod_server
                    .as_ref()
                    .is_some_and(|handle| handle.is_alive())
                && self.has_event_loop())
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
        let _guard = self.activity_update.lock().unwrap();
        self.connection_requested.store(false, Ordering::SeqCst);
        self.repair_state.pause();
    }

    // Host transitions invalidate both greeting requests and established
    // reads. Epochs catch loss followed by recovery between consecutive reads.
    pub(super) async fn event_request<F: Future>(
        &self,
        stop: &AtomicBool,
        epoch: u64,
        future: F,
    ) -> Option<F::Output> {
        if stop.load(Ordering::SeqCst) || self.pod_repair_epoch() != epoch {
            return None;
        }
        tokio::select! {
            biased;
            _ = self.pod_repair_changed(epoch) => None,
            result = future => Some(result),
        }
    }

    pub(super) async fn pod_repair_changed(&self, epoch: u64) {
        let changed = self.repair_state.async_wake.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if self.pod_repair_epoch() == epoch {
            changed.await;
        }
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
        drop(resources);
        self.finish_connection_request();
    }

    pub fn host(&self) -> Host {
        self.host.lock().unwrap().clone()
    }

    pub(super) fn set_status(&self, status: PodConnectionStatus) {
        replace_and_emit(&self.status, status, &self.events_tx, &self.key);
    }

    pub(super) fn mark_event_disconnected(&self, epoch: u64) {
        let _update = self.activity_update.lock().unwrap();
        if self.pod_repair_epoch() == epoch {
            self.set_status(PodConnectionStatus::PodDisconnected);
        }
    }

    pub(super) fn emit_reconnect(&self, event: ReconnectEvent) {
        let _ = self.tx.send(event);
    }

    pub(super) fn apply_greeting_at_epoch(&self, greeting: GreetingState, epoch: u64) -> bool {
        self.update_agent_activity(Some(epoch), || {
            *self.event_http_version.lock().unwrap() = Some(greeting.http_version);
            *self.claude_state.lock().unwrap() = greeting.claude;
            *self.codex_state.lock().unwrap() = greeting.codex;
            self.set_status(PodConnectionStatus::Connected);
        })
    }

    pub(super) fn set_claude_state_at_epoch(&self, state: Option<ClaudeState>, epoch: u64) {
        self.update_agent_activity(Some(epoch), || {
            *self.claude_state.lock().unwrap() = state;
        });
    }

    pub(super) fn set_codex_state_at_epoch(&self, state: Option<CodexState>, epoch: u64) {
        self.update_agent_activity(Some(epoch), || {
            *self.codex_state.lock().unwrap() = state;
        });
    }

    #[cfg(test)]
    pub(super) fn apply_greeting(&self, greeting: GreetingState) {
        self.apply_greeting_at_epoch(greeting, self.pod_repair_epoch());
    }

    #[cfg(test)]
    pub(super) fn set_claude_state(&self, state: Option<ClaudeState>) {
        self.set_claude_state_at_epoch(state, self.pod_repair_epoch());
    }

    #[cfg(test)]
    pub(super) fn set_codex_state(&self, state: Option<CodexState>) {
        self.set_codex_state_at_epoch(state, self.pod_repair_epoch());
    }

    pub(super) fn resume_repair(&self) {
        let _guard = self.activity_update.lock().unwrap();
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
                if handle.container_url == container_url
                    && handle.container_token == container_token
                {
                    handle.bypass.store(bypass, Ordering::Relaxed);
                    return Ok(CodexProxyEndpoint {
                        port: handle.port,
                        token: handle.token.clone(),
                    });
                }
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
            container_url.clone(),
            container_token.clone(),
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
            container_url,
            container_token,
            _cancel_tx: cancel_tx,
        });

        Ok(CodexProxyEndpoint { port, token })
    }

    pub fn notify_host_connected(&self) {
        let _guard = self.activity_update.lock().unwrap();
        if self.status() == PodConnectionStatus::Stopped || !self.wants_reconnect() {
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
        let _guard = self.activity_update.lock().unwrap();
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
        if let Some(proxy) = &resources.pod_server {
            proxy.reset_connections();
        }
        if resources.supervise_git_tunnel {
            resources.git_tunnel = None;
            resources.validate_pod_before_git_tunnel_repair = true;
        }
        drop(resources);
        self.emit_reconnect(ReconnectEvent::Attempting);
    }

    pub fn stop_events(&self) {
        let _guard = self.activity_update.lock().unwrap();
        self.stop_event_loop_locked();
        self.update_agent_activity_locked(None, || {
            *self.claude_state.lock().unwrap() = None;
            *self.codex_state.lock().unwrap() = None;
        });
        self.connection_requested.store(false, Ordering::SeqCst);
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
        let _guard = self.activity_update.lock().unwrap();
        self.stop_event_loop_locked();
    }

    fn stop_event_loop_locked(&self) {
        let mut event_loop = self.event_loop.lock().unwrap();
        if let Some(handle) = event_loop.take() {
            handle.stop.store(true, Ordering::SeqCst);
        }
        self.repair_state.signal();
        *self.event_http_version.lock().unwrap() = None;
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
    pub(super) http_version: reqwest::Version,
    pub(super) claude: Option<ClaudeState>,
    pub(super) codex: Option<CodexState>,
}

pub(super) type PodEventReader = Pin<Box<dyn AsyncBufRead + Send>>;

pub(super) async fn connect_pod_events(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    on_connected: impl FnOnce(),
) -> Result<(PodEventReader, GreetingState)> {
    // Established streams send a heartbeat every 30 seconds. An idle read
    // deadline detects a blackhole even when the local proxy accepts TCP.
    connect_pod_events_with_timeout(client, url, token, Duration::from_secs(45), on_connected).await
}

async fn connect_pod_events_with_timeout(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    read_timeout: Duration,
    on_connected: impl FnOnce(),
) -> Result<(PodEventReader, GreetingState)> {
    // Limit idle reads on this event stream, not on the shared client:
    // executions can legitimately remain silent for longer than this deadline.
    let response = tokio::time::timeout(read_timeout, async {
        match client
            .get(format!("{url}/events"))
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(response) => Ok(response),
            Err(error) => {
                // Pre-exec pod servers only speak HTTP/1. Repeating this GET
                // is safe because it cannot start a command.
                log::debug!("pod event connection failed; trying HTTP/1: {error}");
                crate::exec_stream::client_builder()
                    .http1_only()
                    .build()?
                    .get(format!("{url}/events"))
                    .bearer_auth(token)
                    .send()
                    .await
            }
        }
    })
    .await
    .context("waiting for pod event headers")?
    .context("connecting to pod events")?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(anyhow::anyhow!("pod events returned {status}"));
    }

    // Setup can depend on git repair, so it cannot retain host admission.
    on_connected();
    let http_version = response.version();
    let stream = futures_util::stream::try_unfold(response, move |mut response| async move {
        let chunk = tokio::time::timeout(read_timeout, response.chunk())
            .await
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::TimedOut, error))?
            .map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(chunk.map(|bytes| (bytes, response)))
    });
    let mut reader = BufReader::new(StreamReader::new(Box::pin(stream)));
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| anyhow::anyhow!("reading pod event stream: {e}"))?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "pod event stream closed before state event"
            ));
        }
        if line.trim() == "event: state" {
            let mut data_line = String::new();
            reader
                .read_line(&mut data_line)
                .await
                .context("reading state data")?;
            let mut blank = String::new();
            reader
                .read_line(&mut blank)
                .await
                .context("reading state separator")?;

            let greeting = parse_greeting_state(&data_line, http_version);
            return Ok((Box::pin(reader), greeting));
        }
    }
}

fn parse_greeting_state(data_line: &str, http_version: reqwest::Version) -> GreetingState {
    let Some(json_str) = data_line.trim().strip_prefix("data: ") else {
        return GreetingState {
            http_version,
            claude: None,
            codex: None,
        };
    };
    let Ok(obj) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return GreetingState {
            http_version,
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
    GreetingState {
        http_version,
        claude,
        codex,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Instant;

    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;

    use super::*;
    use crate::config::ContainerEngine;

    #[test]
    fn reconnect_eligibility_requires_processing_or_current_demand() {
        let (events, _) = broadcast::channel(16);
        let pod = PodConnection::new(
            events,
            PodConnectionKey::new("/repo", "pod"),
            Host::Localhost {
                engine: ContainerEngine::Docker,
            },
            "token".into(),
            PodConnectionStatus::HostDisconnected,
            None,
        );
        assert!(!pod.wants_reconnect());
        for (state, expected) in [
            (ClaudeState::Processing, true),
            (ClaudeState::WaitingForInput, false),
            (ClaudeState::AuthError, false),
            (ClaudeState::Stopped, false),
        ] {
            pod.set_claude_state(Some(state));
            assert_eq!(pod.wants_reconnect(), expected, "{state:?}");
        }
        for (state, expected) in [
            (CodexState::Processing, true),
            (CodexState::Idle, false),
            (CodexState::Error, false),
        ] {
            pod.set_codex_state(Some(state));
            assert_eq!(pod.wants_reconnect(), expected, "{state:?}");
        }
        pod.restore_agent_processing();
        assert!(pod.wants_reconnect());
        pod.apply_greeting(GreetingState {
            http_version: reqwest::Version::HTTP_2,
            claude: None,
            codex: Some(CodexState::Idle),
        });
        assert!(!pod.wants_reconnect());
        pod.set_status(PodConnectionStatus::PodDisconnected);
        pod.request_connection();
        assert!(pod.wants_reconnect());
        pod.set_status(PodConnectionStatus::Connected);
        pod.finish_connection_request();
        assert!(!pod.wants_reconnect());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let recorded = observed.clone();
        pod.set_activity_observer(Arc::new(move |_, active| {
            recorded.lock().unwrap().push(active)
        }));
        pod.set_codex_state(Some(CodexState::Processing));
        let old_epoch = pod.pod_repair_epoch();
        pod.stop_events();
        pod.set_codex_state_at_epoch(Some(CodexState::Processing), old_epoch);
        assert!(!pod.agent_is_processing());
        assert_eq!(*observed.lock().unwrap(), [true, false]);
        assert!(pod.begin_event_loop_at_epoch(Some(old_epoch)).is_none());
    }

    #[test]
    fn host_loss_serializes_with_greeting_and_preserves_pending_demand() {
        let (events, _) = broadcast::channel(16);
        let pod = Arc::new(PodConnection::new(
            events,
            PodConnectionKey::new("/repo", "pod"),
            Host::Localhost {
                engine: ContainerEngine::Docker,
            },
            "token".into(),
            PodConnectionStatus::PodDisconnected,
            None,
        ));
        pod.request_connection();
        let epoch = pod.pod_repair_epoch();
        let greeting_barrier = pod.claude_state.lock().unwrap();
        let greeting_pod = pod.clone();
        let greeting = std::thread::spawn(move || {
            greeting_pod.apply_greeting_at_epoch(
                GreetingState {
                    http_version: reqwest::Version::HTTP_2,
                    claude: None,
                    codex: None,
                },
                epoch,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while pod.event_http_version().is_err() {
            assert!(
                Instant::now() < deadline,
                "greeting did not reach publication"
            );
            std::thread::yield_now();
        }
        // The greeting has passed its epoch check but is blocked before state
        // publication. Host loss must serialize with the remaining publication.
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let lost_pod = pod.clone();
        let loss = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            lost_pod.notify_host_disconnected();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        let premature_loss = done_rx.recv_timeout(Duration::from_millis(50));
        drop(greeting_barrier);
        assert!(greeting.join().unwrap());
        loss.join().unwrap();
        assert!(matches!(
            premature_loss,
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        pod.finish_connection_request();
        assert_eq!(pod.status(), PodConnectionStatus::HostDisconnected);
        assert!(pod.wants_reconnect());
        assert!(!pod.apply_greeting_at_epoch(
            GreetingState {
                http_version: reqwest::Version::HTTP_11,
                claude: None,
                codex: None
            },
            epoch,
        ));
        assert_eq!(pod.status(), PodConnectionStatus::HostDisconnected);
        assert_eq!(pod.event_http_version().unwrap(), reqwest::Version::HTTP_2);
        pod.stop_events();
        assert!(pod.event_http_version().is_err());
    }

    #[tokio::test]
    async fn pod_events_idle_timeout_does_not_limit_shared_client_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/events",
                get(|| async {
                    let greeting = futures_util::stream::once(async {
                        Ok::<_, std::io::Error>("event: state\ndata: {}\n\n")
                    });
                    Body::from_stream(greeting.chain(futures_util::stream::pending()))
                }),
            )
            .route(
                "/quiet",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    "done"
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = crate::exec_stream::client_builder().build().unwrap();
        let (mut reader, greeting) = connect_pod_events_with_timeout(
            &client,
            &url,
            "test-token",
            Duration::from_millis(100),
            || {},
        )
        .await
        .unwrap();
        assert_eq!(greeting.http_version, reqwest::Version::HTTP_2);
        let mut event = String::new();
        let quiet = async {
            client
                .get(format!("{url}/quiet"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        };
        let (event_result, quiet_result) = tokio::join!(reader.read_line(&mut event), quiet);
        assert_eq!(
            event_result.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(quiet_result, "done");
        server.abort();
    }

    #[tokio::test]
    async fn pod_events_does_not_retry_http_errors_with_another_protocol() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let received = requests.clone();
        let app = Router::new().route(
            "/events",
            get(move || {
                received.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::BAD_GATEWAY }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = crate::exec_stream::client_builder().build().unwrap();
        let error = connect_pod_events_with_timeout(
            &client,
            &url,
            "test-token",
            Duration::from_secs(1),
            || {},
        )
        .await
        .err()
        .expect("pod event HTTP error must fail the connection");
        assert!(error.to_string().contains("502"), "{error:#}");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn pod_events_cancel_on_repeated_host_loss_and_stop() {
        let (events, _) = broadcast::channel(16);
        let pod = PodConnection::new(
            events,
            PodConnectionKey::new("/repo", "pod"),
            Host::Localhost {
                engine: ContainerEngine::Docker,
            },
            "test-token".into(),
            PodConnectionStatus::Connected,
            None,
        );
        let stop = pod.begin_event_loop().unwrap();
        for _ in 0..2 {
            let epoch = pod.pod_repair_epoch();
            let (started, ready) = tokio::sync::oneshot::channel();
            let request = pod.event_request(&stop, epoch, async {
                started.send(()).unwrap();
                std::future::pending::<()>().await
            });
            let outage = async {
                ready.await.unwrap();
                pod.notify_host_disconnected();
                pod.notify_host_connected();
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(request, outage)
            })
            .await
            .expect("host transition left the event request blocked");
            assert!(result.is_none());
            assert!(pod.event_request(&stop, epoch, async {}).await.is_none());
            assert_eq!(
                pod.event_request(&stop, pod.pod_repair_epoch(), async { 42 })
                    .await,
                Some(42)
            );
        }
        let epoch = pod.pod_repair_epoch();
        let (started, ready) = tokio::sync::oneshot::channel();
        let request = pod.event_request(&stop, epoch, async {
            started.send(()).unwrap();
            std::future::pending::<()>().await
        });
        let shutdown = async {
            ready.await.unwrap();
            pod.stop_events();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(request, shutdown)
        })
        .await
        .expect("stop left the event request blocked");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn pod_events_silent_transport_has_a_read_deadline() {
        // Cover a proxy that accepts TCP without HTTP headers, an incomplete
        // greeting, and a blackhole after the state greeting was delivered.
        for body in [
            "",
            "HTTP/1.1 200 OK\r\n\r\nevent: state\n",
            "HTTP/1.1 200 OK\r\n\r\nevent: state\ndata: {}\n\n",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                stream.write_all(body.as_bytes()).await.unwrap();
                // Stay open until the client's deadline closes its side.
                stream.read_to_end(&mut Vec::new()).await.unwrap();
            });
            let result = tokio::time::timeout(Duration::from_secs(3), async {
                match connect_pod_events_with_timeout(
                    &reqwest::Client::new(),
                    &url,
                    "test-token",
                    Duration::from_millis(200),
                    || {},
                )
                .await
                {
                    Ok((mut reader, _)) => reader
                        .read_line(&mut String::new())
                        .await
                        .map_err(anyhow::Error::from),
                    Err(error) => Err(error),
                }
            })
            .await
            .expect("event request never expired");
            assert!(
                result.is_err(),
                "silent event stream unexpectedly completed"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn pod_events_heartbeats_extend_the_read_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\n\r\nevent: state\ndata: {}\n\n")
                .await
                .unwrap();
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                stream.write_all(b": heartbeat\n").await.unwrap();
            }
        });
        let (mut reader, _) = connect_pod_events_with_timeout(
            &reqwest::Client::new(),
            &url,
            "test-token",
            Duration::from_millis(500),
            || {},
        )
        .await
        .unwrap();
        for _ in 0..10 {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(line, ": heartbeat\n");
        }
        server.await.unwrap();
    }

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
