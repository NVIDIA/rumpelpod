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

use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream as StdUnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio_util::task::AbortOnDropHandle;

use crate::async_command::ProcessGroup;
use crate::async_runtime::RUNTIME;

use crate::config::{ContainerEngine, Host};

pub struct PodmanSshProxy {
    socket_path: PathBuf,
    // The accept task owns active connections, so dropping the proxy also
    // releases SSH processes that an unreachable host cannot finish.
    _task: AbortOnDropHandle<()>,
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

        listener
            .set_nonblocking(true)
            .context("configuring podman proxy listener")?;
        let _runtime = RUNTIME.enter();
        let listener = tokio::net::UnixListener::from_std(listener)?;
        let destination = destination.to_string();
        let task = RUNTIME.spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(result) = connections.join_next() => {
                        if let Err(error) = result { log::error!("podman proxy task failed: {error}"); }
                    }
                    connection = listener.accept() => {
                        let (connection, _) = match connection {
                            Ok(connection) => connection,
                            Err(error) => {
                                log::error!("podman ssh proxy accept failed: {error}");
                                break;
                            }
                        };
                        let destination = destination.clone();
                        connections.spawn(async move {
                            if let Err(error) = serve_connection(connection, &destination).await {
                                log::error!("podman ssh proxy connection to {destination} failed: {error:#}");
                            }
                        });
                    }
                }
            }
        });

        Ok(PodmanSshProxy {
            socket_path,
            _task: AbortOnDropHandle::new(task),
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

/// Pipe one client connection through a fresh `ssh ... dial-stdio`.
///
/// One ssh process per API connection mirrors what the docker CLI does
/// for `-H ssh://`; users get connection reuse the same way, via
/// `ControlMaster` in their SSH config.
async fn serve_connection(conn: UnixStream, destination: &str) -> Result<()> {
    let mut command = Command::new("ssh");
    command.args([
        "-o",
        "BatchMode=yes",
        destination,
        "podman",
        "system",
        "dial-stdio",
    ]);
    bridge_connection(conn, command).await
}

async fn bridge_connection(conn: UnixStream, mut command: Command) -> Result<()> {
    command.as_std_mut().process_group(0);
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ssh podman dial-stdio")?;

    let _group = ProcessGroup::new(&child);
    let mut child_stdin = child.stdin.take().context("ssh child stdin missing")?;
    let mut child_stdout = child.stdout.take().context("ssh child stdout missing")?;
    let mut child_stderr = child.stderr.take().context("ssh child stderr missing")?;
    let conn = conn.into_std()?;
    let disconnect = AsyncFd::with_interest(conn.try_clone()?, Interest::WRITABLE)?;
    let conn = UnixStream::from_std(conn)?;
    let (mut conn_read, mut conn_write) = conn.into_split();
    let mut stderr = String::new();

    let request = async move {
        tokio::io::copy(&mut conn_read, &mut child_stdin).await?;
        drop(child_stdin);
        std::future::pending::<std::io::Result<()>>().await
    };
    let response = async {
        tokio::try_join!(
            async {
                tokio::io::copy(&mut child_stdout, &mut conn_write).await?;
                conn_write.shutdown().await
            },
            child_stderr.read_to_string(&mut stderr),
            child.wait(),
        )
    };
    tokio::select! {
        result = request => { result.context("forwarding podman request")?; }
        result = response => {
            let ((), _, status) = result.context("forwarding podman response")?;
            if !status.success() {
                let stderr = stderr.trim();
                return Err(anyhow::anyhow!("ssh podman dial-stdio exited with {status}: {stderr}"));
            }
        }
        result = wait_disconnected(&disconnect) => { result.context("watching podman client")?; }
    }
    Ok(())
}

async fn wait_disconnected(socket: &AsyncFd<StdUnixStream>) -> std::io::Result<()> {
    // EOF alone can be a half-close: the client may still need its response.
    // A separate registration lets us wait for HUP without consuming either
    // pump's readiness or spinning on the original socket's permanent EOF.
    socket
        .async_io(Interest::WRITABLE, |socket| {
            let mut descriptor = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: 0,
                revents: 0,
            };
            if unsafe { libc::poll(&mut descriptor, 1, 0) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if descriptor.revents & libc::POLLHUP != 0 {
                Ok(())
            } else {
                Err(std::io::ErrorKind::WouldBlock.into())
            }
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn podman_ssh_proxy_half_close_drains_buffered_response() {
        let data = vec![b'x'; 2 * 1024 * 1024];
        let response_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(response_file.path(), &data).unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", "cat >/dev/null; sleep 0.1; cat \"$1\"", "sh"])
            .arg(response_file.path());
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = AbortOnDropHandle::new(tokio::spawn(bridge_connection(server, command)));
        client.write_all(b"request").await.unwrap();
        client.shutdown().await.unwrap();

        // Input EOF still permits a response, including bytes buffered when
        // the child exits. Waiting on child status alone can truncate it.
        let mut response = Vec::new();
        timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, data);
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn podman_ssh_proxy_disconnect_kills_unresponsive_child() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo $$ > \"$1\"; exec sleep 600", "sh"])
            .arg(&pid_file);
        let (client, server) = UnixStream::pair().unwrap();
        let task = AbortOnDropHandle::new(tokio::spawn(bridge_connection(server, command)));
        let pid = timeout(Duration::from_secs(5), async {
            loop {
                match std::fs::read_to_string(&pid_file) {
                    Ok(pid) if !pid.trim().is_empty() => {
                        break Pid::from_raw(pid.trim().parse().unwrap())
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => panic!("reading child PID: {error}"),
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(client);
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                match kill(pid, None) {
                    Err(Errno::ESRCH) => break,
                    Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => panic!("checking cancelled child: {error}"),
                }
            }
        })
        .await
        .unwrap();
    }
}
