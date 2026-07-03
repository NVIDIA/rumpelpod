// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Multiplexed TCP tunnel over exec stdin/stdout.
///
/// Containers (both K8s and Docker) cannot always reach the host's git
/// HTTP server directly.  This module implements a framing protocol that
/// carries multiple TCP streams over a single stdin/stdout pipe
/// established by exec-ing into the container.
///
/// Framing:
///   [stream_id: u32 LE] [frame_type: u8] [payload_len: u32 LE] [payload]
///
/// Frame types:
///   OPEN  (0x01) -- pod -> host, empty payload.  Host opens TCP to target.
///   DATA  (0x02) -- bidirectional.  Max payload 64 KiB.
///   CLOSE (0x03) -- bidirectional, empty payload.  Graceful stream close.
///
/// Pod side (`run_tunnel_server`): binds a loopback TCP listener, assigns
/// stream IDs, and multiplexes accepted connections over stdout.
///
/// Host side (`start_tunnel`): runs `rumpel tunnel-server` inside the
/// container via `Executor::exec_streaming`, demultiplexes frames
/// from stdout, and opens local TCP connections to the target
/// address for each stream.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

use crate::executor::{ExecRequest, ExecStreams, Executor, PodId};
use crate::jitter;

const FRAME_OPEN: u8 = 0x01;
const FRAME_DATA: u8 = 0x02;
const FRAME_CLOSE: u8 = 0x03;

/// Hard cap on a single DATA payload.
const MAX_PAYLOAD: usize = 64 * 1024;

/// Header is stream_id(4) + frame_type(1) + payload_len(4) = 9 bytes.
const HEADER_LEN: usize = 9;

// ── Framing helpers ────────────────────────────────────────────────────

struct Frame {
    stream_id: u32,
    frame_type: u8,
    payload: Vec<u8>,
}

async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Frame> {
    let mut hdr = [0u8; HEADER_LEN];
    reader
        .read_exact(&mut hdr)
        .await
        .context("reading frame header")?;
    let stream_id = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let frame_type = hdr[4];
    let payload_len = u32::from_le_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as usize;
    if payload_len > MAX_PAYLOAD {
        return Err(anyhow::anyhow!(
            "frame payload too large: {payload_len} > {MAX_PAYLOAD} (stream {stream_id})"
        ));
    }
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader
            .read_exact(&mut payload)
            .await
            .context("reading frame payload")?;
    }
    Ok(Frame {
        stream_id,
        frame_type,
        payload,
    })
}

async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, frame: &Frame) -> Result<()> {
    let mut hdr = [0u8; HEADER_LEN];
    hdr[0..4].copy_from_slice(&frame.stream_id.to_le_bytes());
    hdr[4] = frame.frame_type;
    hdr[5..9].copy_from_slice(&(frame.payload.len() as u32).to_le_bytes());
    writer
        .write_all(&hdr)
        .await
        .context("writing frame header")?;
    if !frame.payload.is_empty() {
        writer
            .write_all(&frame.payload)
            .await
            .context("writing frame payload")?;
    }
    writer.flush().await.context("flushing frame")?;
    Ok(())
}

// ── Pod side ───────────────────────────────────────────────────────────

/// Run the in-pod tunnel server.  Binds a TCP listener on loopback,
/// multiplexes accepted connections over stdout, and demultiplexes
/// host replies from stdin.  Never returns under normal operation.
pub fn run_tunnel_server() -> ! {
    let rt = tokio::runtime::Runtime::new().expect("creating tokio runtime");
    rt.block_on(async { run_tunnel_server_async().await });
    std::process::exit(0);
}

async fn run_tunnel_server_async() {
    let path = std::path::Path::new(crate::port_file::TUNNEL_PORT_FILE);

    // Reuse the previously recorded port if the file is still around
    // (e.g. after a tunnel-server restart that kept the container
    // alive).  If the port is held by something else we fail hard --
    // there is no point retrying because nothing else in the container
    // is allowed to squat on a port we are meant to own.
    let preferred = match crate::port_file::read_preferred(path) {
        Ok(p) => p,
        Err(e) => panic!("reading tunnel port file: {e:#}"),
    };

    // Clear the stale file before binding so observers block until we
    // have written the authoritative value.
    if let Err(e) = crate::port_file::remove_if_present(path) {
        panic!("removing stale tunnel port file: {e:#}");
    }

    let addr = match preferred {
        Some(p) => format!("127.0.0.1:{p}"),
        None => "127.0.0.1:0".to_string(),
    };
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => panic!("binding tunnel listener on {addr}: {e}"),
    };

    let actual_port = listener
        .local_addr()
        .expect("getting listener address")
        .port();
    if let Err(e) = crate::port_file::write_atomic(path, actual_port) {
        panic!("writing tunnel port file: {e:#}");
    }
    if let Err(e) = notify_container_server_gateway_port(actual_port).await {
        panic!("notifying container server of tunnel port: {e:#}");
    }
    eprintln!("tunnel listening on port {actual_port}");

    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));

    // Map of stream_id -> sender that feeds data to the TCP write task.
    let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let next_id = Arc::new(AtomicU32::new(1));

    // Stdin reader task: dispatch incoming frames to the right stream.
    // When stdin closes the exec session is gone, so exit immediately
    // to free the TCP listener port.
    let streams_for_stdin = streams.clone();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        loop {
            let frame = match read_frame(&mut stdin).await {
                Ok(f) => f,
                Err(_) => {
                    eprintln!("tunnel-server: exec session closed, exiting");
                    std::process::exit(0);
                }
            };
            match frame.frame_type {
                FRAME_DATA => {
                    let map = streams_for_stdin.lock().await;
                    if let Some(tx) = map.get(&frame.stream_id) {
                        // If the receiver is gone the stream was already torn down.
                        let _ = tx.send(frame.payload).await;
                    }
                }
                FRAME_CLOSE => {
                    // Drop the sender so the TCP write task sees channel close.
                    streams_for_stdin.lock().await.remove(&frame.stream_id);
                }
                _ => {}
            }
        }
    });

    // Accept loop
    loop {
        let (tcp_stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>(64);
        streams.lock().await.insert(id, data_tx);

        let stdout_clone = stdout.clone();
        let streams_clone = streams.clone();

        // Send OPEN
        {
            let mut out = stdout_clone.lock().await;
            if write_frame(
                &mut *out,
                &Frame {
                    stream_id: id,
                    frame_type: FRAME_OPEN,
                    payload: Vec::new(),
                },
            )
            .await
            .is_err()
            {
                break;
            }
        }

        let (read_half, write_half) = tokio::io::split(tcp_stream);

        // TCP read -> DATA frames to stdout; CLOSE on EOF
        let stdout_for_read = stdout_clone.clone();
        let streams_for_read = streams_clone.clone();
        tokio::spawn(async move {
            let mut reader = read_half;
            let mut buf = vec![0u8; MAX_PAYLOAD];
            loop {
                let n = match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut out = stdout_for_read.lock().await;
                if write_frame(
                    &mut *out,
                    &Frame {
                        stream_id: id,
                        frame_type: FRAME_DATA,
                        payload: buf[..n].to_vec(),
                    },
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            // Send CLOSE
            let mut out = stdout_for_read.lock().await;
            let _ = write_frame(
                &mut *out,
                &Frame {
                    stream_id: id,
                    frame_type: FRAME_CLOSE,
                    payload: Vec::new(),
                },
            )
            .await;
            streams_for_read.lock().await.remove(&id);
        });

        // mpsc receiver -> TCP write; shutdown on channel close
        tokio::spawn(async move {
            let mut writer = write_half;
            let mut rx = data_rx;
            while let Some(data) = rx.recv().await {
                if writer.write_all(&data).await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });
    }
}

async fn notify_container_server_gateway_port(tunnel_port: u16) -> Result<()> {
    let server_port_path = std::path::Path::new(crate::port_file::SERVER_PORT_FILE);
    let Some(server_port) = crate::port_file::read_preferred(server_port_path)? else {
        return Ok(());
    };
    let token_path = std::path::Path::new(crate::pod::server::TOKEN_FILE);
    let token = match std::fs::read_to_string(token_path) {
        Ok(token) => token,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            let path = token_path.display();
            return Err(anyhow::Error::new(e).context(format!("reading token file {path}")));
        }
    };
    let token = token.trim();
    if token.is_empty() {
        let path = token_path.display();
        return Err(anyhow::anyhow!("token file {path} is empty"));
    }

    let base_url = format!("http://127.0.0.1:{tunnel_port}");
    let server_url = format!("http://127.0.0.1:{server_port}/gateway/refresh");
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(500))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("building gateway refresh client")?;
    let request = crate::pod::types::RefreshGatewayRequest { base_url };
    let response = match client
        .post(&server_url)
        .bearer_auth(token)
        .json(&request)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) if e.is_connect() => return Ok(()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "notifying container server at {server_url}: {e}"
            ));
        }
    };

    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(anyhow::anyhow!(
        "container server gateway refresh returned {status}: {body}"
    ))
}

// ── Host side ──────────────────────────────────────────────────────────

/// Handle for an active tunnel.  Dropping this cancels the tunnel.
pub struct TunnelHandle {
    alive: Arc<AtomicBool>,
    _cancel_tx: tokio::sync::watch::Sender<bool>,
}

impl TunnelHandle {
    /// Whether the mux task is still running.  Returns false once the
    /// WebSocket/pipe to the pod breaks or the task is cancelled.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

/// Shared mux loop for the host side of the tunnel.  Reads frames from
/// the container's stdout, dispatches them to local TCP connections to
/// `target_addr`, and writes response frames back via the container's
/// stdin.
///
/// `_keepalive` is held for the lifetime of the task (e.g. to prevent
/// the kube `AttachedProcess` from being dropped, which would close the
/// WebSocket).
fn spawn_host_mux(
    mut pod_stdout: impl AsyncRead + Unpin + Send + 'static,
    pod_stdin: impl AsyncWriteExt + Unpin + Send + 'static,
    target_addr: String,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    alive: Arc<AtomicBool>,
    _keepalive: impl Send + 'static,
) {
    let pod_stdin_mu = Arc::new(Mutex::new(pod_stdin));

    type WriteMap = Arc<Mutex<HashMap<u32, tokio::io::WriteHalf<TcpStream>>>>;
    let writes: WriteMap = Arc::new(Mutex::new(HashMap::new()));

    let stdin_mu = pod_stdin_mu.clone();
    let writes_clone = writes.clone();

    tokio::spawn(async move {
        let _keepalive = _keepalive;

        // Periodic keepalive prevents exec session idle timeouts
        // (k3s/k3d tear down inactive SPDY streams after ~5-10s).
        let mut keepalive_interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        keepalive_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                frame_res = read_frame(&mut pod_stdout) => {
                    let frame = match frame_res {
                        Ok(f) => f,
                        Err(e) => {
                            log::debug!("tunnel mux: read error: {e}");
                            break;
                        }
                    };
                    match frame.frame_type {
                        FRAME_OPEN => {
                            let sid = frame.stream_id;
                            // Connect synchronously so the write half is in the
                            // map before we read the next frame (which is likely
                            // a DATA for this same stream).
                            let tcp = match TcpStream::connect(&target_addr).await {
                                Ok(s) => s,
                                Err(e) => {
                                    log::debug!(
                                        "tunnel: failed to connect to {target_addr}: {e}"
                                    );
                                    let mut w = stdin_mu.lock().await;
                                    let _ = write_frame(
                                        &mut *w,
                                        &Frame {
                                            stream_id: sid,
                                            frame_type: FRAME_CLOSE,
                                            payload: Vec::new(),
                                        },
                                    )
                                    .await;
                                    continue;
                                }
                            };
                            let (read_half, write_half) = tokio::io::split(tcp);
                            writes_clone.lock().await.insert(sid, write_half);

                            // Spawn reader: local TCP read -> DATA frames back
                            // to pod, CLOSE on EOF.
                            let stdin_for_reader = stdin_mu.clone();
                            let writes_for_reader = writes_clone.clone();
                            tokio::spawn(async move {
                                let mut reader = read_half;
                                let mut buf = vec![0u8; MAX_PAYLOAD];
                                loop {
                                    let n = match reader.read(&mut buf).await {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => n,
                                    };
                                    let mut w = stdin_for_reader.lock().await;
                                    if write_frame(
                                        &mut *w,
                                        &Frame {
                                            stream_id: sid,
                                            frame_type: FRAME_DATA,
                                            payload: buf[..n].to_vec(),
                                        },
                                    )
                                    .await
                                    .is_err()
                                    {
                                        break;
                                    }
                                }
                                let mut w = stdin_for_reader.lock().await;
                                let _ = write_frame(
                                    &mut *w,
                                    &Frame {
                                        stream_id: sid,
                                        frame_type: FRAME_CLOSE,
                                        payload: Vec::new(),
                                    },
                                )
                                .await;
                                writes_for_reader.lock().await.remove(&sid);
                            });
                        }
                        FRAME_DATA => {
                            let mut map = writes_clone.lock().await;
                            if let Some(writer) = map.get_mut(&frame.stream_id) {
                                if writer.write_all(&frame.payload).await.is_err() {
                                    map.remove(&frame.stream_id);
                                }
                            }
                        }
                        FRAME_CLOSE => {
                            // Dropping the WriteHalf from tokio::io::split
                            // does not close the TcpStream -- both halves
                            // share a BiLock and the ReadHalf is still
                            // pinned in the reader task below.  Shutdown
                            // sends FIN so the server observes EOF and
                            // closes, which lets the reader task exit
                            // and release the fd.
                            let sid = frame.stream_id;
                            let writer = writes_clone.lock().await.remove(&sid);
                            if let Some(mut writer) = writer {
                                if let Err(e) = writer.shutdown().await {
                                    log::error!(
                                        "tunnel mux: shutdown on stream {sid} failed: {e}"
                                    );
                                }
                            }
                        }
                        _ => {
                            let frame_type = frame.frame_type;
                            log::debug!(
                                "tunnel mux: unknown frame type {frame_type}"
                            );
                        }
                    }
                }
                _ = keepalive_interval.tick() => {
                    let mut w = stdin_mu.lock().await;
                    if write_frame(
                        &mut *w,
                        &Frame {
                            stream_id: 0,
                            frame_type: FRAME_DATA,
                            payload: Vec::new(),
                        },
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                _ = cancel_rx.changed() => {
                    break;
                }
            }
        }
        alive.store(false, Ordering::Relaxed);
    });
}

/// Start a tunnel into a pod on either backend.
///
/// Runs `rumpel tunnel-server` inside the pod via `Executor::exec_streaming`,
/// waits for the readiness line on stderr, and spawns a mux task that
/// bridges frames to local TCP connections to `target_addr`.
///
/// Retries on failure indefinitely (the caller can cancel).
pub async fn start_tunnel(
    executor: &Executor,
    pod_id: &PodId,
    target_addr: &str,
) -> Result<TunnelHandle> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match start_tunnel_inner(executor, pod_id, target_addr).await {
            Ok(handle) => return Ok(handle),
            Err(e) => {
                log::warn!(
                    "tunnel to pod '{pod_id}' failed (attempt {attempt}): {e:#}. Retrying..."
                );
                tokio::time::sleep(jitter(std::time::Duration::from_secs(1))).await;
            }
        }
    }
}

async fn start_tunnel_inner(
    executor: &Executor,
    pod_id: &PodId,
    target_addr: &str,
) -> Result<TunnelHandle> {
    // Kill any leftover tunnel-server from a previous run.  We don't
    // care whether the pkill finds anything, just that we don't race
    // a stale process for the loopback port inside the pod.
    let _ = executor
        .exec_async(
            pod_id,
            ExecRequest {
                cmd: vec![
                    "sh".into(),
                    "-c".into(),
                    "pkill -f 'rumpel tunnel-server' 2>/dev/null; sleep 0.5; true".into(),
                ],
                workdir: None,
                env: Vec::new(),
                stdin: None,
            },
        )
        .await;

    let ExecStreams {
        stdin,
        stdout,
        mut stderr,
        keepalive,
    } = executor
        .exec_streaming(
            pod_id,
            vec![
                "/opt/rumpelpod/bin/rumpel".to_string(),
                "tunnel-server".to_string(),
            ],
        )
        .await
        .context("exec-streaming tunnel-server in pod")?;

    // Wait for the "tunnel listening on port N" readiness line on stderr.
    let mut stderr_buf = Vec::new();
    let mut ready = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow::anyhow!(
                "timeout waiting for tunnel-server readiness"
            ));
        }
        let mut tmp = [0u8; 256];
        let n = tokio::time::timeout(remaining, stderr.read(&mut tmp))
            .await
            .context("timeout reading tunnel stderr")?
            .context("reading tunnel stderr")?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "tunnel-server stderr closed before readiness signal"
            ));
        }
        stderr_buf.extend_from_slice(&tmp[..n]);
        if let Ok(s) = std::str::from_utf8(&stderr_buf) {
            // Only consider newline-terminated lines so we don't parse
            // a partial chunk that happens to start with the prefix.
            for line in s.lines() {
                if line.starts_with("tunnel listening on port ") {
                    ready = true;
                    break;
                }
            }
            if ready {
                break;
            }
        }
    }

    // Drain remaining stderr to debug log in the background.
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                        for line in s.lines() {
                            log::debug!("tunnel-server stderr: {line}");
                        }
                    }
                }
            }
        }
    });

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let alive = Arc::new(AtomicBool::new(true));

    spawn_host_mux(
        stdout,
        stdin,
        target_addr.to_string(),
        cancel_rx,
        alive.clone(),
        keepalive,
    );

    Ok(TunnelHandle {
        alive,
        _cancel_tx: cancel_tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    /// FRAME_CLOSE from the pod must half-close the host's outbound TCP
    /// so the server observes EOF and releases the fd.  Without
    /// shutdown() the WriteHalf from tokio::io::split keeps the socket
    /// open; the symptom was 360+ leaked loopback pairs on port 39631
    /// pinned in the daemon process.
    #[tokio::test]
    async fn frame_close_shuts_down_target_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = listener.local_addr().unwrap().to_string();

        // One duplex pair per direction.  host_mux reads `from_pod`
        // (pod's simulated stdout) and writes to `to_pod` (pod's
        // simulated stdin).  We hold the opposite ends.
        let (from_pod_host, mut from_pod_test) = tokio::io::duplex(8192);
        let (to_pod_host, _to_pod_test) = tokio::io::duplex(8192);

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let alive = Arc::new(AtomicBool::new(true));
        spawn_host_mux(
            from_pod_host,
            to_pod_host,
            target_addr,
            cancel_rx,
            alive,
            (),
        );

        write_frame(
            &mut from_pod_test,
            &Frame {
                stream_id: 1,
                frame_type: FRAME_OPEN,
                payload: Vec::new(),
            },
        )
        .await
        .unwrap();

        let (mut accepted, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("target server did not receive a connection")
            .unwrap();

        write_frame(
            &mut from_pod_test,
            &Frame {
                stream_id: 1,
                frame_type: FRAME_CLOSE,
                payload: Vec::new(),
            },
        )
        .await
        .unwrap();

        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), accepted.read(&mut buf))
            .await
            .expect("read on target socket hung: FRAME_CLOSE did not half-close")
            .expect("read failed");
        assert_eq!(n, 0, "expected EOF after FRAME_CLOSE");

        drop(cancel_tx);
    }
}
