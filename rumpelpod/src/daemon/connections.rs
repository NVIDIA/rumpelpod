// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Host and pod connection state, plus the loops that use both.
//!
//! `HostConnection` and `PodConnection` are siblings.  Logic that only
//! needs one of them lives on that type.  Anything that has to see a
//! host *and* a pod lives here, including the pod event loop and the
//! host-event / git-tunnel tasks.

use std::io::BufRead;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use log::{debug, info};
use tokio::sync::broadcast;

use crate::async_runtime::RUNTIME;
use crate::config::Host;
use crate::daemon::host_connection::{
    HostConnection, HostConnectionEvent, HostConnectionEventRx, HostConnectionRegistry, HostStatus,
};
use crate::daemon::pod_connection::{
    connect_pod_events, PodConnection, PodConnectionRegistry, PodConnectionStatus, PodEndpoint,
    PodRepairBackoff,
};
use crate::daemon::protocol::DaemonEvent;
use crate::daemon::reconnect::ReconnectEvent;
use crate::pod::types::{ClaudeState, CodexState};

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
}

impl Connections {
    pub fn new(events_tx: broadcast::Sender<DaemonEvent>) -> Self {
        let (host_events_tx, host_events_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                hosts: HostConnectionRegistry::new(host_events_tx),
                pods: PodConnectionRegistry::new(events_tx),
                host_events_rx: Mutex::new(Some(host_events_rx)),
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
        F: Fn(Arc<PodConnection>) + Send + Sync + 'static,
    {
        let this = self.clone();
        let repair = Arc::new(repair);
        RUNTIME.spawn(async move {
            loop {
                for pod in this.git_tunnels_needing_repair() {
                    let repair = repair.clone();
                    let pod_name = pod.key().pod_name().to_string();
                    std::thread::Builder::new()
                        .name(format!("git-tunnel-{pod_name}"))
                        .spawn(move || repair(pod))
                        .expect("failed to spawn git tunnel supervisor");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    pub fn get_or_create_host(&self, host: &Host) -> Result<Arc<HostConnection>> {
        self.inner.hosts.get_or_create(host)
    }

    pub fn host(&self, host: &Host) -> Option<Arc<HostConnection>> {
        self.inner.hosts.get(host)
    }

    /// Get or create the host and probe it now.  Used by client
    /// interest (`connect`, `enter`) so a down host is woken instead
    /// of waiting out its backoff.
    pub fn ensure_host(&self, host: &Host) -> Result<Arc<HostConnection>> {
        let conn = self.inner.hosts.get_or_create(host)?;
        conn.ensure_connected()?;
        Ok(conn)
    }

    pub fn get_or_create_pod(
        &self,
        repo_path: &Path,
        pod_name: &str,
        host: Host,
        token: String,
    ) -> Result<Arc<PodConnection>> {
        let host_conn = self.inner.hosts.get_or_create(&host)?;
        let initial_status = if host_conn.is_connected() {
            PodConnectionStatus::PodDisconnected
        } else {
            PodConnectionStatus::HostDisconnected
        };
        Ok(self
            .inner
            .pods
            .get_or_create(repo_path, pod_name, host, token, initial_status))
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

    pub fn subscribe_pod(
        &self,
        repo_path: &Path,
        pod_name: &str,
    ) -> Option<broadcast::Receiver<ReconnectEvent>> {
        self.inner.pods.subscribe(repo_path, pod_name)
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
        let Some(endpoint) = pod.endpoint() else {
            return;
        };
        let Some(host) = self.host(&pod.host()) else {
            return;
        };
        let Some(stop) = pod.begin_event_loop() else {
            return;
        };

        pod.set_status(PodConnectionStatus::Connecting);
        let thread_stop = stop.clone();
        let pod_name = pod.key().pod_name().to_string();
        let loop_pod = pod.clone();
        let thread = std::thread::Builder::new()
            .name(format!("pod-connection-{pod_name}"))
            .spawn(move || {
                run_pod_event_loop(loop_pod, host, endpoint, thread_stop);
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

        let mut reader = match connect_pod_events(&endpoint.url, &endpoint.token) {
            Ok((reader, greeting)) => {
                pod.apply_greeting(greeting);
                pod.set_status(PodConnectionStatus::Connected);
                info!("pod connection for '{pod_name}' established");
                pod.emit_reconnect(ReconnectEvent::Connected);
                backoff.reset();
                reader
            }
            Err(e) => {
                if backoff.failures == 0 {
                    info!("pod connection for '{pod_name}' failed; retrying: {e:#}");
                } else {
                    debug!("pod event connection for '{pod_name}' failed: {e:#}");
                }
                pod.set_status(PodConnectionStatus::PodDisconnected);
                pod.emit_reconnect(ReconnectEvent::Failed {
                    error: format!("{e:#}"),
                });
                host.request_probe();
                let Some(delay) = backoff.next_delay() else {
                    log::warn!(
                        "pod connection repair for '{pod_name}' paused after repeated failures"
                    );
                    pod.pause_pod_repair();
                    continue;
                };
                let next_epoch = pod.wait_for_pod_repair_change(repair_epoch, delay);
                if next_epoch != repair_epoch {
                    repair_epoch = next_epoch;
                    backoff.reset();
                }
                continue;
            }
        };

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
            match reader.read_line(&mut line) {
                Ok(0) => {
                    info!("pod event stream for '{pod_name}' closed; reconnecting");
                    break;
                }
                Err(e) => {
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
                                    pod.set_claude_state(cs);
                                }
                            }
                            Some("codex_state") => {
                                if let Ok(xs) = serde_json::from_str::<Option<CodexState>>(data) {
                                    pod.set_codex_state(xs);
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

fn wait_for_pod_repair_resume(
    pod: &PodConnection,
    stop: &AtomicBool,
    repair_epoch: &mut u64,
) -> bool {
    while pod.pod_repair_is_paused() {
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
    use super::*;
    use crate::config::ContainerEngine;
    use crate::daemon::host_connection::HostKey;

    #[test]
    fn host_reconnect_resumes_every_pod_on_that_host() {
        let (daemon_events_tx, mut daemon_events_rx) = broadcast::channel(4);
        let connections = Connections::new(daemon_events_tx);
        let docker_host = Host::Localhost {
            engine: ContainerEngine::Docker,
        };
        let podman_host = Host::Localhost {
            engine: ContainerEngine::Podman,
        };
        let docker_a = connections
            .get_or_create_pod(Path::new("/repo-a"), "a", docker_host.clone(), "a".into())
            .unwrap();
        let docker_b = connections
            .get_or_create_pod(Path::new("/repo-b"), "b", docker_host, "b".into())
            .unwrap();
        let podman = connections
            .get_or_create_pod(Path::new("/repo-c"), "c", podman_host, "c".into())
            .unwrap();

        for connection in [&docker_a, &docker_b, &podman] {
            connection.enable_git_tunnel_supervision();
            connection.mark_pod_validated_for_git_tunnel_repair();
            connection.pause_pod_repair();
        }
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
        assert!(!docker_b.pod_repair_is_paused());
        assert!(podman.pod_repair_is_paused());
        assert!(docker_a.try_schedule_git_tunnel_repair());
        assert!(docker_b.try_schedule_git_tunnel_repair());
        assert!(!podman.try_schedule_git_tunnel_repair());
        let mut connected_pods = Vec::new();
        for _ in 0..2 {
            match daemon_events_rx.try_recv().expect("receive connect event") {
                DaemonEvent::PodStatusChanged { pod, .. } => connected_pods.push(pod),
                event => panic!("unexpected connect event: {event:?}"),
            }
        }
        connected_pods.sort();
        assert_eq!(connected_pods, ["a", "b"]);
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
            .get_or_create_pod(Path::new("/repo"), "local", local_host, "token".into())
            .expect("create local pod");
        local.set_status(PodConnectionStatus::Connected);
        connections.prepare_pod_server_repair(&local);
        assert_eq!(local.status(), PodConnectionStatus::PodDisconnected);

        // No HostConnection for this host, so repair must not treat the pod
        // as reachable.
        let orphan = connections.inner.pods.get_or_create(
            Path::new("/repo"),
            "orphan",
            missing_host,
            "token".into(),
            PodConnectionStatus::Connected,
        );
        connections.prepare_pod_server_repair(&orphan);
        assert_eq!(orphan.status(), PodConnectionStatus::HostDisconnected);
    }
}
