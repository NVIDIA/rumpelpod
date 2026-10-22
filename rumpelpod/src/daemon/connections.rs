// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Host and pod connection state, plus the loops that use both.
//!
//! `HostConnection` and `PodConnection` are siblings.  Logic that only
//! needs one of them lives on that type.  Anything that has to see a
//! host *and* a pod lives here, including the pod event loop and the
//! host-event / git-tunnel tasks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{debug, info};
use tokio::io::AsyncBufReadExt;
use tokio::sync::{broadcast, OwnedSemaphorePermit, Semaphore};

use crate::async_runtime::RUNTIME;
use crate::config::Host;
use crate::daemon::host_connection::{
    HostConnection, HostConnectionEvent, HostConnectionEventRx, HostConnectionRegistry, HostKey,
    HostStatus,
};
use crate::daemon::pod_connection::{
    connect_pod_events, PodConnection, PodConnectionRegistry, PodConnectionStatus, PodEndpoint,
    PodRepairBackoff,
};
use crate::daemon::protocol::DaemonEvent;
use crate::daemon::reconnect::ReconnectEvent;
use crate::pod::types::{ClaudeState, CodexState};

// Successful handshakes can release capacity immediately. Failed or cancelled
// attempts retain their slots briefly so fast failures cannot spin on the host.
const POD_REPAIR_CONCURRENCY: usize = 2;
const POD_REPAIR_FAILURE_COOLDOWN: Duration = Duration::from_millis(500);

struct PodRepairLimiter {
    slots: Arc<Semaphore>,
}

impl PodRepairLimiter {
    fn new() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(POD_REPAIR_CONCURRENCY)),
        }
    }

    async fn acquire(&self) -> PodRepairPermit {
        PodRepairPermit {
            slot: Some(self.slots.clone().acquire_owned().await.unwrap()),
        }
    }
}

struct PodRepairPermit {
    slot: Option<OwnedSemaphorePermit>,
}

impl PodRepairPermit {
    fn connected(mut self) {
        drop(self.slot.take());
    }
}

impl Drop for PodRepairPermit {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            // Cancellation is not evidence of a healthy transport. Keep the
            // cooldown even when the observer gives up on its request.
            RUNTIME.spawn(async move {
                tokio::time::sleep(POD_REPAIR_FAILURE_COOLDOWN).await;
                drop(slot);
            });
        }
    }
}

/// Shared host and pod connection tables.  Cheap to clone: the maps
/// live behind an `Arc` so tasks can hold a handle.  `DaemonServer`
/// stores this by value.
#[derive(Clone)]
pub struct Connections {
    inner: Arc<Inner>,
}

struct Inner {
    hosts: HostConnectionRegistry,
    pods: PodConnectionRegistry,
    host_events_rx: Mutex<Option<HostConnectionEventRx>>,
    pod_repairs: Mutex<HashMap<HostKey, Arc<PodRepairLimiter>>>,
}

impl Connections {
    pub fn new(events_tx: broadcast::Sender<DaemonEvent>) -> Self {
        let (host_events_tx, host_events_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                hosts: HostConnectionRegistry::new(host_events_tx),
                pods: PodConnectionRegistry::new(events_tx),
                host_events_rx: Mutex::new(Some(host_events_rx)),
                pod_repairs: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Start the host-event reader.  Call once.
    pub fn start(&self) {
        let rx = self
            .inner
            .host_events_rx
            .lock()
            .unwrap()
            .take()
            .expect("Connections::start called twice");
        let inner = self.inner.clone();
        RUNTIME.spawn(host_event_reader(inner, rx));
    }

    /// Poll for git tunnels that need repair and hand each one to
    /// `repair`.  `repair` is daemon code (it needs the DB and the
    /// localhost git port); this loop only decides *when* to call it.
    pub fn start_git_supervisor<F>(&self, repair: F)
    where
        F: Fn(&Arc<PodConnection>, &HostKey) -> Result<()> + Send + Sync + 'static,
    {
        let this = self.clone();
        let repair = Arc::new(repair);
        RUNTIME.spawn(async move {
            loop {
                for pod in this.git_tunnels_needing_repair() {
                    let host = this.host(&pod.host()).expect("scheduled repair has a host");
                    let limiter = this.pod_repair_limiter(&host);
                    let repair = repair.clone();
                    RUNTIME.spawn(supervise_git_tunnel(pod, host, limiter, repair));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    pub fn get_or_create_host(&self, host: &Host) -> Result<Arc<HostConnection>> {
        self.inner.hosts.get_or_create(host)
    }

    fn pod_repair_limiter(&self, host: &HostConnection) -> Arc<PodRepairLimiter> {
        self.inner
            .pod_repairs
            .lock()
            .unwrap()
            .entry(host.key())
            .or_insert_with(|| Arc::new(PodRepairLimiter::new()))
            .clone()
    }

    pub fn host(&self, host: &Host) -> Option<Arc<HostConnection>> {
        self.inner.hosts.get(host)
    }

    /// Get or create the host and probe it now.  Used by client
    /// interest (`connect`, `enter`) so a down host is woken instead
    /// of waiting out its backoff.
    pub async fn ensure_host_async(&self, host: &Host) -> Result<Arc<HostConnection>> {
        let conn = self.inner.hosts.get_or_create(host)?;
        conn.ensure_connected_async().await?;
        Ok(conn)
    }

    #[cfg(test)]
    pub fn get_or_create_pod(
        &self,
        repo_path: &Path,
        pod_name: &str,
        host: Host,
        token: String,
        configured_ssh_keys: Option<&[PathBuf]>,
    ) -> Result<Arc<PodConnection>> {
        crate::async_runtime::block_on(self.get_or_create_pod_async(
            repo_path,
            pod_name,
            host,
            token,
            configured_ssh_keys,
        ))
    }

    pub async fn get_or_create_pod_async(
        &self,
        repo_path: &Path,
        pod_name: &str,
        host: Host,
        token: String,
        configured_ssh_keys: Option<&[PathBuf]>,
    ) -> Result<Arc<PodConnection>> {
        let host_conn = self.inner.hosts.get_or_create(&host)?;
        let initial_status = if host_conn.is_connected() {
            PodConnectionStatus::PodDisconnected
        } else {
            PodConnectionStatus::HostDisconnected
        };
        self.inner
            .pods
            .get_or_create_async(
                repo_path,
                pod_name,
                host,
                token,
                initial_status,
                configured_ssh_keys,
            )
            .await
    }

    pub fn pod(&self, repo_path: &Path, pod_name: &str) -> Option<Arc<PodConnection>> {
        self.inner.pods.get(repo_path, pod_name)
    }

    pub fn remove_pod(&self, repo_path: &Path, pod_name: &str) -> Option<Arc<PodConnection>> {
        self.inner.pods.remove(repo_path, pod_name)
    }

    pub fn stop_pod_events(&self, repo_path: &Path, pod_name: &str) {
        self.inner.pods.stop_events(repo_path, pod_name);
    }

    pub fn pod_endpoint(&self, repo_path: &Path, pod_name: &str) -> Option<PodEndpoint> {
        self.inner.pods.endpoint(repo_path, pod_name)
    }

    pub fn claude_state(&self, repo_path: &Path, pod_name: &str) -> Option<ClaudeState> {
        self.inner.pods.claude_state(repo_path, pod_name)
    }

    pub fn codex_state(&self, repo_path: &Path, pod_name: &str) -> Option<CodexState> {
        self.inner.pods.codex_state(repo_path, pod_name)
    }

    pub fn pod_status(&self, repo_path: &Path, pod_name: &str) -> Option<PodConnectionStatus> {
        self.inner.pods.status(repo_path, pod_name)
    }

    pub fn pod_host_is_connected(&self, pod: &PodConnection) -> bool {
        self.host(&pod.host())
            .is_some_and(|host| host.is_connected())
    }

    /// A failed pod probe is not proof that the host is down.  Drop the
    /// pod server and re-check the host before choosing a status.
    pub fn prepare_pod_server_repair(&self, pod: &PodConnection) {
        pod.remove_pod_server();
        pod.resume_repair();
        let status = if self.pod_host_is_connected(pod) {
            PodConnectionStatus::PodDisconnected
        } else {
            PodConnectionStatus::HostDisconnected
        };
        pod.set_status(status);
        pod.emit_reconnect(ReconnectEvent::Attempting);
    }

    pub fn ensure_event_loop(&self, pod: &Arc<PodConnection>) {
        pod.request_connection();
        self.start_event_loop(pod, None);
    }

    pub fn ensure_repaired_event_loop(&self, pod: &Arc<PodConnection>, epoch: u64) {
        self.start_event_loop(pod, Some(epoch));
    }

    fn start_event_loop(&self, pod: &Arc<PodConnection>, epoch: Option<u64>) {
        let Some(endpoint) = pod.endpoint() else {
            return;
        };
        let Some(host) = self.host(&pod.host()) else {
            return;
        };
        let stop = match epoch {
            Some(epoch) => pod.begin_event_loop_at_epoch(Some(epoch)),
            None => pod.begin_event_loop(),
        };
        let Some(stop) = stop else {
            return;
        };

        let thread_stop = stop.clone();
        let pod_name = pod.key().pod_name().to_string();
        let loop_pod = pod.clone();
        let limiter = self.pod_repair_limiter(&host);
        let thread = std::thread::Builder::new()
            .name(format!("pod-connection-{pod_name}"))
            .spawn(move || {
                run_pod_event_loop(loop_pod, host, limiter, endpoint, thread_stop);
            })
            .expect("failed to spawn pod connection event loop");
        pod.finish_event_loop(thread);
    }

    fn git_tunnels_needing_repair(&self) -> Vec<Arc<PodConnection>> {
        self.inner
            .pods
            .all()
            .into_iter()
            .filter(|pod| self.pod_host_is_connected(pod) && pod.try_schedule_git_tunnel_repair())
            .collect()
    }
}

async fn supervise_git_tunnel<F>(
    pod: Arc<PodConnection>,
    host: Arc<HostConnection>,
    limiter: Arc<PodRepairLimiter>,
    repair: Arc<F>,
) where
    F: Fn(&Arc<PodConnection>, &HostKey) -> Result<()> + Send + Sync + 'static,
{
    let host_key = host.key();
    let mut backoff = PodRepairBackoff::new();
    let mut repair_epoch = pod.pod_repair_epoch();
    loop {
        if !pod.git_tunnel_supervision_enabled()
            || pod.pod_repair_is_paused()
            || !pod.wants_reconnect()
            || (pod.git_tunnel_is_alive() && pod.has_alive_pod_server() && pod.has_event_loop())
            || !host.is_connected()
            || HostKey::from_host(&pod.host()) != host_key
        {
            break;
        }

        let permit = tokio::select! {
            biased;
            _ = pod.pod_repair_changed(repair_epoch) => {
                repair_epoch = pod.pod_repair_epoch();
                backoff.reset();
                continue;
            }
            permit = limiter.acquire() => permit,
        };
        if !host.is_connected()
            || !pod.wants_reconnect()
            || HostKey::from_host(&pod.host()) != host_key
        {
            break;
        }
        // Waiting for admission or backoff must not allocate one OS thread per
        // pod. Blocking workers are only needed for the transport work itself.
        let attempt_pod = pod.clone();
        let attempt_repair = repair.clone();
        let attempt_host = host_key.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = attempt_repair(&attempt_pod, &attempt_host);
            if result.is_ok() {
                permit.connected();
            }
            result
        })
        .await
        .expect("git tunnel repair task panicked");
        match result {
            Ok(()) => break,
            Err(e) => {
                let name = pod.key().pod_name();
                log::warn!("git tunnel repair for pod '{name}' failed: {e:#}");
                let Some(delay) = backoff.next_delay() else {
                    log::warn!("git tunnel repair for pod '{name}' paused after repeated failures");
                    pod.pause_pod_repair();
                    break;
                };
                tokio::select! {
                    _ = pod.pod_repair_changed(repair_epoch) => {
                        repair_epoch = pod.pod_repair_epoch();
                        backoff.reset();
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    pod.finish_git_tunnel_repair();
}

async fn host_event_reader(inner: Arc<Inner>, mut rx: HostConnectionEventRx) {
    while let Some(event) = rx.recv().await {
        match event {
            HostConnectionEvent::Connected(key) => {
                inner.pods.notify_host_connected(&key);
            }
            HostConnectionEvent::Disconnected(key) => {
                inner.pods.notify_host_disconnected(&key);
            }
            HostConnectionEvent::GaveUp(key) => {
                inner.hosts.remove(&key);
                inner.pods.notify_host_disconnected(&key);
            }
        }
    }
}

fn run_pod_event_loop(
    pod: Arc<PodConnection>,
    host: Arc<HostConnection>,
    limiter: Arc<PodRepairLimiter>,
    endpoint: PodEndpoint,
    stop: Arc<AtomicBool>,
) {
    let pod_name = pod.key().pod_name().to_string();
    let mut host_rx = host.subscribe();
    let mut backoff = PodRepairBackoff::new();
    let mut repair_epoch = pod.pod_repair_epoch();

    loop {
        if !wait_for_pod_repair_resume(&pod, &stop, &mut repair_epoch) {
            return;
        }

        pod.emit_reconnect(ReconnectEvent::Attempting);
        host.request_probe();
        let host_was_disconnected = host.status() == HostStatus::Disconnected;
        if !wait_for_host(&mut host_rx, &stop, &pod) {
            return;
        }
        if host_was_disconnected {
            backoff.reset();
            repair_epoch = pod.pod_repair_epoch();
        }
        pod.set_status(PodConnectionStatus::Connecting);
        pod.emit_reconnect(ReconnectEvent::HostConnected);

        let connection_epoch = pod.pod_repair_epoch();
        let Some(permit) = crate::async_runtime::block_on(pod.event_request(
            &stop,
            connection_epoch,
            limiter.acquire(),
        )) else {
            continue;
        };
        if !host.is_connected() || !pod.wants_reconnect() {
            continue;
        }
        let Some(connected) = crate::async_runtime::block_on(pod.event_request(
            &stop,
            connection_epoch,
            connect_pod_events(&pod.http_client, &endpoint.url, &endpoint.token, || {
                permit.connected();
            }),
        )) else {
            continue;
        };
        let mut reader = match connected {
            Ok((reader, greeting)) => {
                if !pod.apply_greeting_at_epoch(greeting, connection_epoch) {
                    continue;
                }
                pod.finish_connection_request();
                info!("pod connection for '{pod_name}' established");
                reader
            }
            Err(e) => {
                if backoff.failures == 0 {
                    info!("pod connection for '{pod_name}' failed; retrying: {e:#}");
                } else {
                    debug!("pod event connection for '{pod_name}' failed: {e:#}");
                }
                pod.mark_event_disconnected(connection_epoch);
                pod.emit_reconnect(ReconnectEvent::Failed {
                    error: format!("{e:#}"),
                });
                host.request_probe();
                wait_after_pod_failure(&pod, &mut backoff, &mut repair_epoch);
                continue;
            }
        };

        let connected_at = Instant::now();
        let mut pending_event: Option<String> = None;
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if host.status() == HostStatus::Disconnected {
                pod.set_status(PodConnectionStatus::HostDisconnected);
                pod.emit_reconnect(ReconnectEvent::Attempting);
                break;
            }
            let mut line = String::new();
            let Some(read) = crate::async_runtime::block_on(pod.event_request(
                &stop,
                connection_epoch,
                reader.read_line(&mut line),
            )) else {
                break;
            };
            match read {
                Ok(0) => {
                    pod.mark_event_disconnected(connection_epoch);
                    info!("pod event stream for '{pod_name}' closed; reconnecting");
                    break;
                }
                Err(e) => {
                    pod.mark_event_disconnected(connection_epoch);
                    info!("pod event stream for '{pod_name}' failed; reconnecting: {e}");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if let Some(event_type) = trimmed.strip_prefix("event: ") {
                        pending_event = Some(event_type.to_string());
                    } else if let Some(data) = trimmed.strip_prefix("data: ") {
                        match pending_event.as_deref() {
                            Some("claude_state") => {
                                if let Ok(cs) = serde_json::from_str::<Option<ClaudeState>>(data) {
                                    pod.set_claude_state_at_epoch(cs, connection_epoch);
                                }
                            }
                            Some("codex_state") => {
                                if let Ok(xs) = serde_json::from_str::<Option<CodexState>>(data) {
                                    pod.set_codex_state_at_epoch(xs, connection_epoch);
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
        if host.is_connected() && pod.pod_repair_epoch() == connection_epoch {
            // A greeting followed immediately by EOF is not sustained recovery.
            if connected_at.elapsed() >= Duration::from_secs(30) {
                backoff.reset();
            }
            wait_after_pod_failure(&pod, &mut backoff, &mut repair_epoch);
        }
    }
}

fn wait_after_pod_failure(pod: &PodConnection, backoff: &mut PodRepairBackoff, epoch: &mut u64) {
    let Some(delay) = backoff.next_delay() else {
        let name = pod.key().pod_name();
        log::warn!("pod connection repair for '{name}' paused after repeated failures");
        pod.pause_pod_repair();
        return;
    };
    let next_epoch = pod.wait_for_pod_repair_change(*epoch, delay);
    if next_epoch != *epoch {
        *epoch = next_epoch;
        backoff.reset();
    }
}

fn wait_for_pod_repair_resume(
    pod: &PodConnection,
    stop: &AtomicBool,
    repair_epoch: &mut u64,
) -> bool {
    while pod.pod_repair_is_paused() || !pod.wants_reconnect() {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        *repair_epoch = pod.wait_for_pod_repair_change(*repair_epoch, Duration::from_secs(1));
    }
    !stop.load(Ordering::SeqCst)
}

fn wait_for_host(
    host_rx: &mut tokio::sync::watch::Receiver<HostStatus>,
    stop: &AtomicBool,
    pod: &PodConnection,
) -> bool {
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        if *host_rx.borrow() == HostStatus::Connected {
            return true;
        }
        pod.set_status(PodConnectionStatus::HostDisconnected);
        let _ = RUNTIME.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), host_rx.changed()).await
        });
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::routing::get;
    use axum::Router;
    use futures_util::StreamExt;
    use tokio::net::TcpListener;
    use tokio::time::Instant;

    use super::*;
    use crate::config::ContainerEngine;

    #[tokio::test]
    async fn pod_repair_admission_bounds_concurrency_and_cools_failed_attempts() {
        let limiter = PodRepairLimiter::new();
        let first = limiter.acquire().await;
        let second = limiter.acquire().await;

        // A slow handshake must keep its slot even without failures.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), limiter.acquire())
                .await
                .is_err()
        );
        let started = Instant::now();
        drop(first);
        let third = limiter.acquire().await;
        assert!(started.elapsed() >= POD_REPAIR_FAILURE_COOLDOWN);

        // Cancellation and errors both release capacity only after cooldown.
        let started = Instant::now();
        drop(second);
        let fourth = limiter.acquire().await;
        assert!(started.elapsed() >= POD_REPAIR_FAILURE_COOLDOWN);
        third.connected();
        fourth.connected();
    }

    #[tokio::test]
    async fn pod_repair_admission_fast_success_has_no_fixed_delay() {
        let limiter = PodRepairLimiter::new();
        tokio::time::timeout(Duration::from_secs(1), async {
            for _ in 0..100 {
                limiter.acquire().await.connected();
            }
        })
        .await
        .expect("healthy recovery must follow handshake speed, not a fixed rate");
    }

    #[tokio::test]
    async fn reconnect_stream_that_closes_after_greeting_keeps_failure_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = PodEndpoint {
            url: format!("http://{}", listener.local_addr().unwrap()),
            token: "token".into(),
        };
        let (attempt_tx, mut attempts) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/events",
            get(move || {
                let attempt_tx = attempt_tx.clone();
                async move {
                    attempt_tx.send(Instant::now()).unwrap();
                    "event: state\ndata: {\"codex_state\":\"processing\"}\n\n"
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (events, _) = broadcast::channel(8);
        let pods = PodConnectionRegistry::new(events);
        let pod = pods
            .get_or_create_async(
                Path::new("/repo"),
                "flapping",
                Host::Localhost {
                    engine: ContainerEngine::Docker,
                },
                "token".into(),
                PodConnectionStatus::PodDisconnected,
                None,
            )
            .await
            .unwrap();
        pod.set_codex_state(Some(CodexState::Processing));
        let stop = pod.begin_event_loop().unwrap();
        let thread_pod = pod.clone();
        let thread = std::thread::spawn(move || {
            run_pod_event_loop(
                thread_pod,
                Arc::new(HostConnection::connected_for_test()),
                Arc::new(PodRepairLimiter::new()),
                endpoint,
                stop,
            )
        });
        let times = tokio::time::timeout(Duration::from_secs(5), async {
            let first = attempts.recv().await.unwrap();
            let second = attempts.recv().await.unwrap();
            let third = attempts.recv().await.unwrap();
            [first, second, third]
        })
        .await
        .expect("flapping stream never retried");
        pod.stop_events();
        tokio::task::spawn_blocking(move || thread.join().unwrap())
            .await
            .unwrap();
        server.abort();
        assert!(times[1] - times[0] >= Duration::from_millis(500));
        assert!(times[2] - times[1] >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn pod_repair_admission_releases_transport_before_setup_needs_git() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (git_ready, git_rx) = tokio::sync::watch::channel(false);
        let app = Router::new().route(
            "/events",
            get(move || {
                let mut git_rx = git_rx.clone();
                async move {
                    let progress = futures_util::stream::once(async {
                        Ok::<_, std::io::Error>("event: progress\ndata: waiting for git\n\n")
                    });
                    let greeting = futures_util::stream::once(async move {
                        git_rx.wait_for(|ready| *ready).await.unwrap();
                        Ok::<_, std::io::Error>("event: state\ndata: {}\n\n")
                    });
                    Body::from_stream(
                        progress
                            .chain(greeting)
                            .chain(futures_util::stream::pending()),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let limiter = Arc::new(PodRepairLimiter::new());
        let (connected_tx, mut connected_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut events = Vec::new();
        for _ in 0..POD_REPAIR_CONCURRENCY {
            let limiter = limiter.clone();
            let url = url.clone();
            let connected_tx = connected_tx.clone();
            events.push(tokio::spawn(async move {
                let client = crate::exec_stream::client_builder().build().unwrap();
                let permit = limiter.acquire().await;
                connect_pod_events(&client, &url, "token", || {
                    permit.connected();
                    connected_tx.send(()).unwrap();
                })
                .await
                .unwrap();
            }));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..POD_REPAIR_CONCURRENCY {
                connected_rx.recv().await.unwrap();
            }
        })
        .await
        .expect("transport admission must be released before the readiness greeting");
        assert!(events.iter().all(|event| !event.is_finished()));

        // Both streams need git before they can become ready. Their transport
        // slots must be free so the work that unblocks setup can run.
        let git_permit = tokio::time::timeout(Duration::from_secs(1), limiter.acquire())
            .await
            .expect("setup streams deadlocked git repair behind their own admission");
        git_ready.send(true).unwrap();
        git_permit.connected();
        for event in events {
            tokio::time::timeout(Duration::from_secs(1), event)
                .await
                .unwrap()
                .unwrap();
        }
        server.abort();
    }

    #[tokio::test]
    async fn pod_repair_admission_is_shared_per_host_and_cancelled_on_host_loss() {
        let (events_tx, _) = broadcast::channel(4);
        let connections = Connections::new(events_tx);
        let docker = Host::Localhost {
            engine: ContainerEngine::Docker,
        };
        let docker_host = connections.get_or_create_host(&docker).unwrap();
        let podman_host = connections
            .get_or_create_host(&Host::Localhost {
                engine: ContainerEngine::Podman,
            })
            .unwrap();
        let limiter = connections.pod_repair_limiter(&docker_host);
        assert!(Arc::ptr_eq(
            &limiter,
            &connections.pod_repair_limiter(&docker_host),
        ));
        let first = limiter.acquire().await;
        let second = limiter.acquire().await;
        let other = connections.pod_repair_limiter(&podman_host);
        let independent = tokio::time::timeout(Duration::from_secs(1), other.acquire())
            .await
            .expect("another host must not queue behind these repairs");
        drop(independent);

        let pod = connections
            .get_or_create_pod_async(Path::new("/repo"), "queued", docker, "token".into(), None)
            .await
            .unwrap();
        let stop = AtomicBool::new(false);
        let epoch = pod.pod_repair_epoch();
        let queued = pod.event_request(&stop, epoch, limiter.acquire());
        tokio::pin!(queued);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut queued)
            .await
            .is_err());
        pod.notify_host_disconnected();
        assert!(queued.await.is_none());
        drop(first);
        drop(second);
        let recovered = tokio::time::timeout(Duration::from_secs(1), limiter.acquire())
            .await
            .expect("cancelled repairs must release admission");
        drop(recovered);
    }

    #[tokio::test]
    async fn pod_repair_admission_does_not_follow_a_pod_to_another_host() {
        let (events_tx, _) = broadcast::channel(4);
        let connections = Connections::new(events_tx);
        let docker = Host::Localhost {
            engine: ContainerEngine::Docker,
        };
        let host = Arc::new(HostConnection::connected_for_test());
        let pod = connections
            .inner
            .pods
            .get_or_create_async(
                Path::new("/repo"),
                "moving",
                docker,
                "token".into(),
                PodConnectionStatus::PodDisconnected,
                None,
            )
            .await
            .unwrap();
        pod.enable_git_tunnel_supervision();
        pod.request_connection();
        assert!(pod.try_schedule_git_tunnel_repair());
        let limiter = connections.pod_repair_limiter(&host);
        let first = limiter.acquire().await;
        let second = limiter.acquire().await;
        let repair = supervise_git_tunnel(
            pod.clone(),
            host,
            limiter,
            Arc::new(|_: &Arc<PodConnection>, _: &HostKey| -> Result<()> {
                panic!("a moved pod must not use the old host's repair budget")
            }),
        );
        tokio::pin!(repair);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut repair)
            .await
            .is_err());

        // Restored remote pods need git repair without having an event loop
        // whose cancellation would otherwise wake this queued supervisor.
        pod.update_host_and_token(
            Host::Ssh {
                ssh_destination: "new-host".into(),
                engine: ContainerEngine::Docker,
            },
            "token".into(),
        );
        drop(first);
        drop(second);
        tokio::time::timeout(Duration::from_secs(1), repair)
            .await
            .expect("stale repair must release the scheduling flag");
        assert!(pod.try_schedule_git_tunnel_repair());
    }

    #[test]
    fn host_reconnect_only_resumes_processing_or_requested_pods() {
        let (daemon_events_tx, mut daemon_events_rx) = broadcast::channel(4);
        let connections = Connections::new(daemon_events_tx);
        let docker_host = Host::Localhost {
            engine: ContainerEngine::Docker,
        };
        let podman_host = Host::Localhost {
            engine: ContainerEngine::Podman,
        };
        let docker_a = connections
            .get_or_create_pod(
                Path::new("/repo-a"),
                "a",
                docker_host.clone(),
                "a".into(),
                None,
            )
            .unwrap();
        let docker_b = connections
            .get_or_create_pod(Path::new("/repo-b"), "b", docker_host, "b".into(), None)
            .unwrap();
        let podman = connections
            .get_or_create_pod(Path::new("/repo-c"), "c", podman_host, "c".into(), None)
            .unwrap();

        for connection in [&docker_a, &docker_b, &podman] {
            connection.enable_git_tunnel_supervision();
            connection.mark_pod_validated_for_git_tunnel_repair();
            connection.pause_pod_repair();
        }
        docker_a.set_codex_state(Some(CodexState::Processing));
        podman.set_claude_state(Some(ClaudeState::Processing));
        while daemon_events_rx.try_recv().is_ok() {}

        let docker_key = HostKey::Localhost {
            engine: ContainerEngine::Docker,
        };
        connections.inner.pods.notify_host_disconnected(&docker_key);

        assert!(docker_a.validate_pod_before_git_tunnel_repair());
        assert!(docker_b.validate_pod_before_git_tunnel_repair());
        assert!(!podman.validate_pod_before_git_tunnel_repair());
        let mut disconnected_pods = Vec::new();
        for _ in 0..2 {
            match daemon_events_rx
                .try_recv()
                .expect("receive disconnect event")
            {
                DaemonEvent::PodStatusChanged { pod, .. } => disconnected_pods.push(pod),
                event => panic!("unexpected disconnect event: {event:?}"),
            }
        }
        disconnected_pods.sort();
        assert_eq!(disconnected_pods, ["a", "b"]);

        connections.inner.pods.notify_host_connected(&docker_key);

        assert!(!docker_a.pod_repair_is_paused());
        assert!(docker_b.pod_repair_is_paused());
        assert!(podman.pod_repair_is_paused());
        assert!(docker_a.try_schedule_git_tunnel_repair());
        assert!(!docker_b.try_schedule_git_tunnel_repair());
        assert!(!podman.try_schedule_git_tunnel_repair());
        let mut connected_pods = Vec::new();
        for _ in 0..1 {
            match daemon_events_rx.try_recv().expect("receive connect event") {
                DaemonEvent::PodStatusChanged { pod, .. } => connected_pods.push(pod),
                event => panic!("unexpected connect event: {event:?}"),
            }
        }
        connected_pods.sort();
        assert_eq!(connected_pods, ["a"]);
        assert!(daemon_events_rx.try_recv().is_err());

        let subscriber = docker_b.subscribe();
        assert!(!docker_b.pod_repair_is_paused());
        assert!(docker_b.try_schedule_git_tunnel_repair());
        drop(subscriber);
        assert!(!docker_b.wants_reconnect());

        docker_b.request_connection();
        assert!(docker_b.wants_reconnect());
        docker_b.disable_git_tunnel_supervision();
        docker_b.set_status(PodConnectionStatus::Connected);
        docker_b.finish_connection_request();
        assert!(!docker_b.wants_reconnect());
    }

    #[test]
    fn prepare_pod_server_repair_uses_host_liveness() {
        let (events_tx, _events_rx) = broadcast::channel(8);
        let connections = Connections::new(events_tx);
        let local_host = Host::Localhost {
            engine: ContainerEngine::Docker,
        };
        let missing_host = Host::Ssh {
            ssh_destination: "nobody@127.0.0.1".into(),
            engine: ContainerEngine::Docker,
        };

        let local = connections
            .get_or_create_pod(
                Path::new("/repo"),
                "local",
                local_host,
                "token".into(),
                None,
            )
            .expect("create local pod");
        local.set_status(PodConnectionStatus::Connected);
        connections.prepare_pod_server_repair(&local);
        assert_eq!(local.status(), PodConnectionStatus::PodDisconnected);

        // No HostConnection for this host, so repair must not treat the pod
        // as reachable.
        let orphan = connections
            .inner
            .pods
            .get_or_create(
                Path::new("/repo"),
                "orphan",
                missing_host,
                "token".into(),
                PodConnectionStatus::Connected,
                None,
            )
            .expect("create orphan pod");
        connections.prepare_pod_server_repair(&orphan);
        assert_eq!(orphan.status(), PodConnectionStatus::HostDisconnected);
    }
}
