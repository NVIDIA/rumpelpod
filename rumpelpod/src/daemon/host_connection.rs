// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-host connection objects.
//!
//! One `HostConnection` per `Host` (localhost, ssh remote, k8s
//! cluster).  The connection owns whatever state is needed to talk
//! to that host:
//!
//! * `Localhost`: nothing.  Always considered up.
//! * `Ssh`: direct `ssh ... docker system dial-stdio` probes; Docker
//!   clients create their own SSH-backed transports.
//! * `Kubernetes`: a cached `kube` client.
//!
//! Callers (typically `Executor::new`) can trigger a short liveness
//! probe before using a remote host.  Each remote connection also runs
//! a single background monitor that owns reconnection and liveness for
//! that host: one retry loop per host, not one per pod.  The monitor
//! publishes liveness on a `watch` channel that pod loops wait on, and
//! `request_probe` lets a waiter ask for an immediate check instead of
//! waiting out the heartbeat.
//!
//! Connections know nothing about pods or executors.  They publish
//! liveness on a per-connection `watch` channel and also emit
//! `HostConnectionEvent`s on a daemon-wide mpsc channel; the daemon
//! is the single reader and decides what to do per pod on
//! `Connected`/`Disconnected` transitions.

use std::collections::HashMap;
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use log::debug;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::timeout;

use crate::async_runtime::RUNTIME;
use crate::config::Host;
use crate::k8s::K8sClient;

/// Identity of a host: collapses `Host::Kubernetes`'s extra fields
/// (registry, builder, node_selector, tolerations) down to what
/// actually determines which API server we are talking to.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum HostKey {
    Localhost,
    Ssh { destination: String },
    Kubernetes { context: String, namespace: String },
}

impl HostKey {
    pub fn from_host(host: &Host) -> Self {
        match host {
            Host::Localhost => HostKey::Localhost,
            Host::Ssh { ssh_destination } => HostKey::Ssh {
                destination: ssh_destination.clone(),
            },
            Host::Kubernetes {
                context, namespace, ..
            } => HostKey::Kubernetes {
                context: context.clone(),
                namespace: namespace.clone(),
            },
        }
    }
}

impl std::fmt::Display for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostKey::Localhost => write!(f, "localhost"),
            HostKey::Ssh { destination } => write!(f, "ssh://{destination}"),
            HostKey::Kubernetes { context, namespace } => {
                write!(f, "k8s:{context}/{namespace}")
            }
        }
    }
}

/// Liveness of a host connection, published on a per-connection
/// `watch` channel.  Pod loops subscribe and wait for `Connected`
/// before attempting their pod endpoint, so the single per-host
/// monitor owns reconnection and pods merely follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStatus {
    Connected,
    Disconnected,
}

/// Events emitted to the daemon when a connection's state flips.
///
/// `GaveUp` is reserved for a future eviction path (no producer
/// today) so the central reader can already match on it.
#[derive(Debug, Clone)]
pub enum HostConnectionEvent {
    Connected(HostKey),
    Disconnected(HostKey),
    #[allow(dead_code)]
    GaveUp(HostKey),
}

pub type HostConnectionEventTx = mpsc::UnboundedSender<HostConnectionEvent>;
pub type HostConnectionEventRx = mpsc::UnboundedReceiver<HostConnectionEvent>;

/// Daemon-wide registry of host connections, deduplicating live
/// connections by `HostKey`.  The registry caches weak references so
/// an idle host does not keep a background monitor alive forever.
/// All connections share a single mpsc sender so the daemon's central
/// reader can match host events to per-pod state.
pub struct HostConnectionRegistry {
    events_tx: HostConnectionEventTx,
    conns: Mutex<HashMap<HostKey, Weak<HostConnection>>>,
}

impl HostConnectionRegistry {
    pub fn new(events_tx: HostConnectionEventTx) -> Self {
        Self {
            events_tx,
            conns: Mutex::new(HashMap::new()),
        }
    }

    /// Look up an existing connection or build a new one for `host`.
    /// New remote connections start out down; their background monitor
    /// brings them up, and callers can also force an immediate probe
    /// via `HostConnection::ensure_connected` (e.g. `Executor::new`).
    pub fn get_or_create(&self, host: &Host) -> Result<Arc<HostConnection>> {
        let key = HostKey::from_host(host);
        let mut conns = self.conns.lock().unwrap();
        if let Some(conn) = conns.get(&key).and_then(Weak::upgrade) {
            return Ok(conn);
        }
        let conn = Arc::new(HostConnection::new(host, self.events_tx.clone())?);
        conns.insert(key, Arc::downgrade(&conn));
        Ok(conn)
    }

    /// Return a live connection for `host` without creating one.  Used
    /// by paths like `list_pods` that should not implicitly start new
    /// remote connections.
    pub fn get(&self, host: &Host) -> Option<Arc<HostConnection>> {
        let key = HostKey::from_host(host);
        let mut conns = self.conns.lock().unwrap();
        let conn = conns.get(&key).and_then(Weak::upgrade);
        if conn.is_none() {
            conns.remove(&key);
        }
        conn
    }

    /// Remove the connection for `host` from the registry.  The
    /// underlying `Arc<HostConnection>` may live on if other Arcs
    /// are held; drop happens when the last reference goes away.
    /// Used by the central reader on `GaveUp` events.
    pub fn remove(&self, key: &HostKey) -> Option<Arc<HostConnection>> {
        self.conns
            .lock()
            .unwrap()
            .remove(key)
            .and_then(|c| c.upgrade())
    }
}

/// How often the background monitors verify liveness and re-attempt
/// bring-up when down.
const HEALTH_INTERVAL: Duration = Duration::from_secs(15);

/// SSH monitor reconnect backoff while the host is down.  Capped low so
/// the user's terminal reconnects promptly once the host is reachable.
const SSH_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const SSH_MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Per-host connection handle, behind `Arc` in the registry.
pub enum HostConnection {
    Localhost(Arc<LocalhostConnection>),
    Ssh(Arc<SshConnection>),
    Kubernetes(Arc<K8sConnection>),
}

impl HostConnection {
    /// Spawn a new connection.  Localhost is trivial and emits a
    /// single `Connected` event; SSH and Kubernetes each spawn a
    /// background monitor that owns liveness and reconnection.
    pub fn new(host: &Host, events_tx: HostConnectionEventTx) -> Result<Self> {
        match host {
            Host::Localhost => Ok(HostConnection::Localhost(LocalhostConnection::new(
                events_tx,
            ))),
            Host::Ssh { ssh_destination } => Ok(HostConnection::Ssh(SshConnection::new(
                ssh_destination.clone(),
                events_tx,
            ))),
            Host::Kubernetes {
                context, namespace, ..
            } => Ok(HostConnection::Kubernetes(K8sConnection::new(
                context.clone(),
                namespace.clone(),
                events_tx,
            ))),
        }
    }

    pub fn key(&self) -> HostKey {
        match self {
            HostConnection::Localhost(_) => HostKey::Localhost,
            HostConnection::Ssh(c) => c.key(),
            HostConnection::Kubernetes(c) => c.key(),
        }
    }

    /// Whether the connection currently has a live handle.  Cheap
    /// snapshot; does not attempt to bring the connection up.
    pub fn is_connected(&self) -> bool {
        self.status() == HostStatus::Connected
    }

    /// Current liveness snapshot.
    pub fn status(&self) -> HostStatus {
        match self {
            HostConnection::Localhost(_) => HostStatus::Connected,
            HostConnection::Ssh(c) => c.status(),
            HostConnection::Kubernetes(c) => c.status(),
        }
    }

    /// Subscribe to liveness transitions for this host.
    pub fn subscribe(&self) -> watch::Receiver<HostStatus> {
        match self {
            HostConnection::Localhost(c) => c.subscribe(),
            HostConnection::Ssh(c) => c.subscribe(),
            HostConnection::Kubernetes(c) => c.subscribe(),
        }
    }

    /// Ask the per-host monitor to probe now instead of waiting out its
    /// heartbeat.  No-op for localhost.
    pub fn request_probe(&self) {
        match self {
            HostConnection::Localhost(_) => {}
            HostConnection::Ssh(c) => c.request_probe(),
            HostConnection::Kubernetes(c) => c.request_probe(),
        }
    }

    /// Verify the host connection.  Localhost is trivial, SSH runs a
    /// real dial-stdio ping, and Kubernetes reuses a cached client when
    /// it still answers.
    pub fn ensure_connected(&self) -> Result<()> {
        match self {
            HostConnection::Localhost(_) => Ok(()),
            HostConnection::Ssh(c) => c.ensure_connected(),
            HostConnection::Kubernetes(c) => c.ensure_client().map(|_| ()),
        }
    }
}

/// Aborts the held tokio task on drop so monitors are torn down
/// when the connection is dropped from the registry.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ============================================================
// Localhost
// ============================================================

pub struct LocalhostConnection {
    // Held only so a future caller can read the events tx if
    // needed; today the constructor uses it once and the field
    // exists to mirror the other variants.
    _events_tx: HostConnectionEventTx,
    /// Permanently `Connected`; lets pod loops subscribe uniformly.
    status_tx: watch::Sender<HostStatus>,
}

impl LocalhostConnection {
    fn new(events_tx: HostConnectionEventTx) -> Arc<Self> {
        // Emit Connected once so the daemon's reader handles
        // localhost uniformly with the other variants.
        let _ = events_tx.send(HostConnectionEvent::Connected(HostKey::Localhost));
        let (status_tx, _) = watch::channel(HostStatus::Connected);
        Arc::new(Self {
            _events_tx: events_tx,
            status_tx,
        })
    }

    fn subscribe(&self) -> watch::Receiver<HostStatus> {
        self.status_tx.subscribe()
    }
}

// ============================================================
// SSH
// ============================================================

pub struct SshConnection {
    destination: String,
    events_tx: HostConnectionEventTx,
    /// Single source of truth for liveness; updated by `ensure_connected`
    /// and observed by pod loops waiting for the host to come back.
    status_tx: watch::Sender<HostStatus>,
    /// Wakes the background monitor for an immediate probe.
    probe: Arc<Notify>,
    /// Serializes probes so concurrent callers do not dial in parallel.
    bring_up: Mutex<()>,
    /// The single per-host retry/liveness loop.  Aborted on drop.
    _monitor: AbortOnDrop,
}

impl SshConnection {
    fn new(destination: String, events_tx: HostConnectionEventTx) -> Arc<Self> {
        let (status_tx, _) = watch::channel(HostStatus::Disconnected);
        let probe = Arc::new(Notify::new());
        Arc::new_cyclic(|weak: &Weak<SshConnection>| {
            let task = RUNTIME.spawn(ssh_monitor(weak.clone(), probe.clone()));
            SshConnection {
                destination,
                events_tx,
                status_tx,
                probe,
                bring_up: Mutex::new(()),
                _monitor: AbortOnDrop(task),
            }
        })
    }

    pub fn key(&self) -> HostKey {
        HostKey::Ssh {
            destination: self.destination.clone(),
        }
    }

    pub fn is_connected(&self) -> bool {
        *self.status_tx.borrow() == HostStatus::Connected
    }

    pub fn subscribe(&self) -> watch::Receiver<HostStatus> {
        self.status_tx.subscribe()
    }

    pub fn status(&self) -> HostStatus {
        *self.status_tx.borrow()
    }

    pub fn request_probe(&self) {
        self.probe.notify_one();
    }

    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// Update liveness, emitting a host event only on a real transition.
    fn set_status(&self, status: HostStatus) {
        let changed = self.status_tx.send_if_modified(|current| {
            if *current != status {
                *current = status;
                true
            } else {
                false
            }
        });
        if changed {
            let event = match status {
                HostStatus::Connected => HostConnectionEvent::Connected(self.key()),
                HostStatus::Disconnected => HostConnectionEvent::Disconnected(self.key()),
            };
            let _ = self.events_tx.send(event);
        }
    }

    /// Verify that Docker answers through `ssh ... docker system
    /// dial-stdio` and record the result.  Both the demand-driven
    /// bring-up (`Executor::new`) and the background monitor go through
    /// here; `bring_up` serializes them so there is at most one dial in
    /// flight per host.
    pub fn ensure_connected(&self) -> Result<()> {
        let _guard = self.bring_up.lock().unwrap();

        if ping_ssh_docker(&self.destination) {
            self.set_status(HostStatus::Connected);
            return Ok(());
        }

        self.set_status(HostStatus::Disconnected);
        Err(anyhow::anyhow!(
            "SSH Docker transport to {} failed to answer /_ping within {:?}. \
             Check SSH configuration, remote Docker, and Docker socket permissions.",
            self.key(),
            PING_TIMEOUT
        ))
    }
}

/// Background monitor for an SSH connection: the single retry and
/// liveness loop per host.  Probes immediately, heartbeats while
/// connected, and backs off while down.  `request_probe` wakes it early
/// (e.g. when a pod loop sees its endpoint fail).  Holds only a `Weak`
/// so the connection can drop and abort it.
async fn ssh_monitor(weak: Weak<SshConnection>, probe: Arc<Notify>) {
    let mut backoff = SSH_INITIAL_BACKOFF;
    loop {
        let Some(conn) = weak.upgrade() else { return };
        let key = conn.key();
        let conn_for_probe = conn.clone();
        let connected =
            match tokio::task::spawn_blocking(move || conn_for_probe.ensure_connected()).await {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    debug!("{key}: ssh monitor probe failed: {e:#}");
                    false
                }
                Err(e) => {
                    debug!("{key}: ssh monitor task join error: {e}");
                    false
                }
            };
        // Release the strong reference before sleeping so the connection
        // can be dropped (and this task aborted) while the monitor idles.
        drop(conn);

        let wait = if connected {
            backoff = SSH_INITIAL_BACKOFF;
            HEALTH_INTERVAL
        } else {
            let current = backoff;
            backoff = std::cmp::min(backoff.saturating_mul(2), SSH_MAX_BACKOFF);
            current
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = probe.notified() => {}
        }
    }
}

fn ssh_dial_stdio_args(destination: &str) -> Vec<OsString> {
    vec![
        OsString::from("-o"),
        OsString::from("BatchMode=yes"),
        OsString::from(destination),
        OsString::from("docker"),
        OsString::from("system"),
        OsString::from("dial-stdio"),
    ]
}

/// Hard ceiling on the docker `/_ping` round-trip over SSH.
const PING_TIMEOUT: Duration = Duration::from_secs(30);

fn ping_ssh_docker(destination: &str) -> bool {
    let destination = destination.to_string();
    match RUNTIME
        .block_on(async { timeout(PING_TIMEOUT, ping_ssh_docker_inner(destination)).await })
    {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            debug!("ssh docker ping failed: {e:#}");
            false
        }
        Err(_) => {
            debug!("ssh docker ping timed out after {PING_TIMEOUT:?}");
            false
        }
    }
}

async fn ping_ssh_docker_inner(destination: String) -> Result<()> {
    let mut child = TokioCommand::new("ssh")
        .args(ssh_dial_stdio_args(&destination))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("spawning ssh docker dial-stdio")?;

    let mut stdin = child.stdin.take().context("ssh child stdin missing")?;
    let mut stdout = child.stdout.take().context("ssh child stdout missing")?;

    let request = "GET /_ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stdin
        .write_all(request.as_bytes())
        .await
        .context("writing docker ping request")?;
    stdin
        .shutdown()
        .await
        .context("closing docker ping request")?;

    let mut response = [0u8; 256];
    let n = stdout
        .read(&mut response)
        .await
        .context("reading docker ping response")?;
    let status = child.wait().await.context("waiting for ssh docker ping")?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "ssh docker dial-stdio exited with {status}"
        ));
    }
    if n == 0 {
        return Err(anyhow::anyhow!("empty docker ping response"));
    }

    let response_str = String::from_utf8_lossy(&response[..n]);
    if response_str.starts_with("HTTP/1.1 200") || response_str.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err(anyhow::anyhow!("unexpected docker ping response"))
    }
}

// ============================================================
// Kubernetes
// ============================================================

pub struct K8sConnection {
    context: String,
    namespace: String,
    events_tx: HostConnectionEventTx,
    state: Mutex<K8sState>,
    /// Single source of truth for liveness, mirroring `state.client`.
    status_tx: watch::Sender<HostStatus>,
    /// Wakes the background monitor for an immediate probe.
    probe: Arc<Notify>,
    bring_up: Mutex<()>,
    _monitor: AbortOnDrop,
}

struct K8sState {
    client: Option<K8sClient>,
}

impl K8sConnection {
    fn new(context: String, namespace: String, events_tx: HostConnectionEventTx) -> Arc<Self> {
        let (status_tx, _) = watch::channel(HostStatus::Disconnected);
        let probe = Arc::new(Notify::new());
        Arc::new_cyclic(|weak: &Weak<K8sConnection>| {
            let task = RUNTIME.spawn(k8s_monitor(weak.clone(), probe.clone()));
            K8sConnection {
                context,
                namespace,
                events_tx,
                state: Mutex::new(K8sState { client: None }),
                status_tx,
                probe,
                bring_up: Mutex::new(()),
                _monitor: AbortOnDrop(task),
            }
        })
    }

    pub fn key(&self) -> HostKey {
        HostKey::Kubernetes {
            context: self.context.clone(),
            namespace: self.namespace.clone(),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<HostStatus> {
        self.status_tx.subscribe()
    }

    pub fn status(&self) -> HostStatus {
        *self.status_tx.borrow()
    }

    pub fn request_probe(&self) {
        self.probe.notify_one();
    }

    /// Update liveness, emitting a host event only on a real transition.
    fn set_status(&self, status: HostStatus) {
        let changed = self.status_tx.send_if_modified(|current| {
            if *current != status {
                *current = status;
                true
            } else {
                false
            }
        });
        if changed {
            let event = match status {
                HostStatus::Connected => HostConnectionEvent::Connected(self.key()),
                HostStatus::Disconnected => HostConnectionEvent::Disconnected(self.key()),
            };
            let _ = self.events_tx.send(event);
        }
    }

    /// Return a usable `K8sClient`, building one if the connection
    /// is currently down or the cached client no longer answers
    /// the API server.  The returned client is a clone of the
    /// cached one (kube's `Client` is `Clone` and shares the
    /// underlying connection pool).
    pub fn ensure_client(&self) -> Result<K8sClient> {
        let _guard = self.bring_up.lock().unwrap();

        let cached = {
            let state = self.state.lock().unwrap();
            state.client.clone()
        };

        if let Some(client) = cached {
            if check_k8s_alive(&client).is_ok() {
                self.set_status(HostStatus::Connected);
                return Ok(client);
            }
            // Stale client: clear and mark disconnected.
            self.state.lock().unwrap().client = None;
            self.set_status(HostStatus::Disconnected);
        }

        let client = match K8sClient::new(&self.context, &self.namespace) {
            Ok(client) => client,
            Err(e) => {
                self.set_status(HostStatus::Disconnected);
                return Err(e).context("building kube client");
            }
        };
        // Cheap probe so we don't cache a client that fails on its
        // first real call.
        if let Err(e) = check_k8s_alive(&client) {
            self.set_status(HostStatus::Disconnected);
            return Err(e);
        }
        {
            let mut state = self.state.lock().unwrap();
            state.client = Some(client.clone());
        }
        self.set_status(HostStatus::Connected);
        Ok(client)
    }
}

/// Background monitor for a k8s connection: a periodic
/// `ensure_client` ping that doubles as liveness check and
/// reconnect attempt.  `request_probe` wakes it early.
async fn k8s_monitor(weak: Weak<K8sConnection>, probe: Arc<Notify>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(HEALTH_INTERVAL) => {}
            _ = probe.notified() => {}
        }
        let Some(conn) = weak.upgrade() else { return };
        let key = conn.key();
        let conn_clone = conn.clone();
        match tokio::task::spawn_blocking(move || conn_clone.ensure_client()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => debug!("{key}: k8s monitor bring-up failed: {e:#}"),
            Err(e) => debug!("{key}: k8s monitor task join error: {e}"),
        }
    }
}

/// Cheap synchronous liveness check for a `K8sClient`.
fn check_k8s_alive(client: &K8sClient) -> Result<()> {
    crate::async_runtime::block_on(check_k8s_alive_async(client))
}

async fn check_k8s_alive_async(client: &K8sClient) -> Result<()> {
    let fut = client.client().apiserver_version();
    timeout(Duration::from_secs(5), fut)
        .await
        .context("apiserver version timed out")?
        .context("apiserver version failed")?;
    Ok(())
}

#[cfg(test)]
fn args_to_strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect()
}

#[cfg(test)]
mod ssh_tests {
    use super::*;

    fn assert_no_rumpelpod_ssh_policy(args: &[String]) {
        let joined = args.join(" ");
        for forbidden in [
            "ControlPath",
            "ControlMaster",
            "ControlPersist",
            "ServerAliveInterval",
            "ServerAliveCountMax",
        ] {
            assert!(
                !joined.contains(forbidden),
                "ssh args should not include {forbidden}: {joined}"
            );
        }
    }

    #[test]
    fn ssh_args_do_not_include_port() {
        let args = args_to_strings(&ssh_dial_stdio_args("dev"));

        assert_eq!(
            args,
            vec![
                "-o",
                "BatchMode=yes",
                "dev",
                "docker",
                "system",
                "dial-stdio"
            ]
        );
        assert_no_rumpelpod_ssh_policy(&args);
        assert!(!args.iter().any(|arg| arg == "-p"));
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn registry_does_not_keep_idle_connection_alive() {
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let registry = HostConnectionRegistry::new(events_tx);

        let conn = registry.get_or_create(&Host::Localhost).unwrap();
        let same = registry.get_or_create(&Host::Localhost).unwrap();
        assert!(Arc::ptr_eq(&conn, &same));
        drop(same);

        assert!(registry.get(&Host::Localhost).is_some());
        drop(conn);
        assert!(registry.get(&Host::Localhost).is_none());
    }
}
