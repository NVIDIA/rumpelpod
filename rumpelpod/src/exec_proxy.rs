// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Host-side exec proxy for reaching container-serve via the
//! executor's streaming exec primitive.
//!
//! Instead of routing through bridge IPs, SSH port forwards, or
//! kubectl port-forward, we exec `rumpel tcp-proxy --port <port>` into
//! the pod for each TCP connection.  The proxy bridges the
//! connection's bytes over the exec's stdin/stdout.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::executor::{Executor, PodId};

/// Handle for an active exec proxy listener.  Dropping cancels the
/// accept loop.
pub struct ExecProxyHandle {
    /// Local port the proxy listener is bound to.
    pub port: u16,
    alive: Arc<AtomicBool>,
    _cancel_tx: tokio::sync::watch::Sender<bool>,
}

impl ExecProxyHandle {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

/// Start a local TCP listener that proxies each accepted connection
/// through an exec of `rumpel tcp-proxy` inside the pod.
///
/// Works on both backends: the `Executor` picks between docker and
/// kubectl subprocesses.
pub async fn start_exec_proxy(
    executor: Executor,
    pod_id: PodId,
    container_port: u16,
) -> Result<ExecProxyHandle> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding exec proxy listener")?;
    start_exec_proxy_on_listener(listener, executor, pod_id, container_port)
}

/// Like [`start_exec_proxy`] but takes a caller-supplied listener.
///
/// Lets callers pick the host port up front (e.g. rebinding a port
/// recorded in the database so URLs stay stable across daemon
/// restarts).  Safe to call from sync code: the accept loop is
/// spawned on the shared daemon runtime rather than the current
/// thread's (possibly absent) tokio context.
pub fn start_exec_proxy_on_listener(
    listener: TcpListener,
    executor: Executor,
    pod_id: PodId,
    container_port: u16,
) -> Result<ExecProxyHandle> {
    let port = listener.local_addr()?.port();

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let alive = Arc::new(AtomicBool::new(true));
    let alive2 = alive.clone();

    crate::async_runtime::RUNTIME.spawn(async move {
        accept_loop(
            listener,
            executor,
            pod_id,
            container_port,
            cancel_rx,
            alive2,
        )
        .await;
    });

    Ok(ExecProxyHandle {
        port,
        alive,
        _cancel_tx: cancel_tx,
    })
}

async fn accept_loop(
    listener: TcpListener,
    executor: Executor,
    pod_id: PodId,
    container_port: u16,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    alive: Arc<AtomicBool>,
) {
    loop {
        let stream = tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        log::error!("exec proxy accept error: {e}");
                        break;
                    }
                }
            }
            _ = cancel_rx.changed() => break,
        };

        let executor = executor.clone();
        let pod_id = pod_id.clone();

        tokio::spawn(async move {
            if let Err(e) = bridge_connection(stream, &executor, &pod_id, container_port).await {
                log::debug!("exec proxy bridge ended: {e:#}");
            }
        });
    }

    alive.store(false, Ordering::Relaxed);
}

async fn bridge_connection(
    tcp_stream: tokio::net::TcpStream,
    executor: &Executor,
    pod_id: &PodId,
    container_port: u16,
) -> Result<()> {
    let streams = executor
        .exec_streaming(
            pod_id,
            vec![
                crate::daemon::RUMPEL_CONTAINER_BIN.to_string(),
                "tcp-proxy".to_string(),
                "--port".to_string(),
                container_port.to_string(),
            ],
        )
        .await
        .context("exec-streaming tcp-proxy in pod")?;

    let crate::executor::ExecStreams {
        mut stdin,
        mut stdout,
        mut stderr,
        keepalive,
    } = streams;

    // Forward stderr to debug log so tcp-proxy diagnostics surface
    // without corrupting the tunneled byte stream.
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                        for line in s.lines() {
                            log::debug!("tcp-proxy stderr: {line}");
                        }
                    }
                }
            }
        }
    });

    let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();

    // exec stdout -> tcp write
    let h1 = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tcp_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tcp_write.shutdown().await;
    });

    // tcp read -> exec stdin
    let h2 = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = stdin.shutdown().await;
    });

    let _ = h1.await;
    let _ = h2.await;
    // Keep the backend session alive until both directions have drained;
    // dropping here tears down the docker exec / kubectl subprocess.
    drop(keepalive);

    Ok(())
}
