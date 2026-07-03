// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Local unix socket proxy for Podman on SSH hosts.
//!
//! Podman's own `ssh://` transport dials the remote API socket with an
//! OpenSSH streamlocal forward, which locked-down servers (e.g. behind
//! Teleport) reject, and its built-in SSH client ignores `~/.ssh/config`.
//! Docker avoids both problems by running `docker system dial-stdio` on
//! the remote through the plain exec channel.  Podman ships the same
//! subcommand but the podman CLI cannot use it directly, so this proxy
//! bridges the gap: a local unix socket that pipes every connection
//! through `ssh <dest> podman system dial-stdio`.  The podman CLI then
//! targets the proxy with `--url unix://...`.
//!
//! `podman system dial-stdio` on the remote connects to whatever API
//! socket the SSH user's podman resolves by default (rootless socket,
//! or `CONTAINER_HOST` from e.g. sshd `SetEnv`).

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::{ContainerEngine, Host};

pub struct PodmanSshProxy {
    socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    /// Holds the socket file.  Drop only signals the accept loop; it
    /// does not join it, so the loop may briefly outlive the dir (its
    /// listener fd stays valid).
    _dir: tempfile::TempDir,
}

impl PodmanSshProxy {
    /// Bind the proxy socket and start the accept loop.
    ///
    /// No SSH connection is opened until a client connects, so this is
    /// cheap and cannot fail on an unreachable host.
    pub fn start(destination: &str) -> Result<Self> {
        // Short prefix under /tmp: macOS caps unix socket paths at 104
        // bytes, which longer runtime or temp dirs can exceed.
        let dir = tempfile::TempDir::with_prefix_in("rp-podman-", "/tmp")
            .context("creating podman ssh proxy dir")?;
        // Connecting here grants full API access to the remote podman,
        // and tempfile honors the umask, so restrict the directory
        // explicitly.  Before the bind, so the socket is never
        // reachable through a laxer directory.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .context("restricting podman ssh proxy dir")?;
        let socket_path = dir.path().join("podman.sock");
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("binding podman ssh proxy {}", socket_path.display()))?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown = shutdown.clone();
        let accept_destination = destination.to_string();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if accept_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let conn = match conn {
                    Ok(conn) => conn,
                    Err(e) => {
                        // The loop ends here; later podman calls get a
                        // bare connection error, so leave a trace.
                        eprintln!("podman ssh proxy accept failed: {e}");
                        break;
                    }
                };
                let destination = accept_destination.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_connection(conn, &destination) {
                        // The podman CLI only reports an opaque broken
                        // connection; the ssh stderr is here.
                        eprintln!("podman ssh proxy connection to {destination} failed: {e:#}");
                    }
                });
            }
        });

        Ok(PodmanSshProxy {
            socket_path,
            shutdown,
            _dir: dir,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Start a proxy when `host` is an SSH Podman host, for callers
    /// that invoke the podman CLI directly rather than through an
    /// `Executor`.  Returns `None` for every other host kind.
    pub fn for_host(host: &Host) -> Result<Option<Self>> {
        match host {
            Host::Ssh {
                ssh_destination,
                engine: ContainerEngine::Podman,
            } => Ok(Some(Self::start(ssh_destination)?)),
            Host::Ssh {
                engine: ContainerEngine::Docker,
                ..
            } => Ok(None),
            Host::Ssh {
                engine: ContainerEngine::Auto,
                ..
            } => {
                panic!("container engine auto remained after resolve")
            }
            Host::Localhost { .. } | Host::Kubernetes { .. } => Ok(None),
        }
    }
}

impl Drop for PodmanSshProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock the accept loop so it observes the flag.  In-flight
        // connections keep their ssh child until either side closes.
        let _ = UnixStream::connect(&self.socket_path);
    }
}

/// Forward bytes as they arrive until EOF or an error on either side.
///
/// Deliberately not `std::io::copy`: its Linux splice specialization
/// for socket/pipe pairs can sit on data instead of forwarding it,
/// which stalls request/response traffic on a connection that is
/// still open in both directions.
fn pump(mut from: impl Read, mut to: impl Write) {
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if to.write_all(&buf[..n]).is_err() {
            break;
        }
        if to.flush().is_err() {
            break;
        }
    }
}

/// Pipe one client connection through a fresh `ssh ... dial-stdio`.
///
/// One ssh process per API connection mirrors what the docker CLI does
/// for `-H ssh://`; users get connection reuse the same way, via
/// `ControlMaster` in their SSH config.
fn serve_connection(conn: UnixStream, destination: &str) -> Result<()> {
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            destination,
            "podman",
            "system",
            "dial-stdio",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ssh podman dial-stdio")?;

    let child_stdin = child.stdin.take().context("ssh child stdin missing")?;
    let child_stdout = child.stdout.take().context("ssh child stdout missing")?;
    let mut child_stderr = child.stderr.take().context("ssh child stderr missing")?;

    // Drain stderr concurrently so a chatty remote cannot fill the pipe
    // and stall ssh.
    let stderr_pump = std::thread::spawn(move || {
        let mut stderr = String::new();
        let _ = child_stderr.read_to_string(&mut stderr);
        stderr
    });

    let conn_read = conn.try_clone().context("cloning proxy connection")?;
    let request_pump = std::thread::spawn(move || {
        pump(conn_read, child_stdin);
        // Dropping stdin sends EOF; dial-stdio then closes the remote
        // side and the response pump finishes.
    });

    let conn_write = conn.try_clone().context("cloning proxy connection")?;
    pump(child_stdout, conn_write);
    let _ = conn.shutdown(std::net::Shutdown::Both);
    let _ = request_pump.join();

    let stderr = stderr_pump.join().unwrap_or_default();
    let status = child.wait().context("waiting for ssh podman dial-stdio")?;
    if !status.success() {
        let stderr = stderr.trim();
        return Err(anyhow::anyhow!(
            "ssh podman dial-stdio exited with {status}: {stderr}"
        ));
    }
    Ok(())
}
