// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::env::VarError;
use std::io::{self, IsTerminal};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::{ReaderStream, StreamReader};
use tokio_util::task::AbortOnDropHandle;

use crate::async_runtime::block_on;
use crate::cli::ExecCommand;
use crate::daemon::protocol::{ClientContext, ConnectPodRequest, Daemon, DaemonClient};
use crate::daemon::{self, DaemonServer};
use crate::exec_pty::RawTerminal;
use crate::exec_stream::{
    self, ExecSignal, ExecSpec, ExecutionId, Frame, ResizeRequest, SignalRequest, TerminalSize,
    TerminalSpec, CHUNK_SIZE, CONTENT_TYPE, UNKNOWN_OUTCOME,
};
use crate::git::get_repo_root;

#[derive(Serialize, Deserialize)]
struct ExecRequest {
    connection: ConnectPodRequest,
    spec: ExecSpec,
}

pub(crate) fn exec(command: &ExecCommand) -> Result<u8> {
    let interactive = command.tty && io::stdin().is_terminal();
    let terminal_fd = if !command.tty {
        None
    } else if io::stdin().is_terminal() {
        Some(libc::STDIN_FILENO)
    } else if io::stdout().is_terminal() {
        Some(libc::STDOUT_FILENO)
    } else if io::stderr().is_terminal() {
        Some(libc::STDERR_FILENO)
    } else {
        None
    };
    let repo_path = get_repo_root()?;
    let relative_workdir = std::env::current_dir()?
        .strip_prefix(&repo_path)
        .context("current directory is outside the repository")?
        .to_path_buf();
    let socket = daemon::socket_path()?;
    let exit_code = block_on(async {
        let window_change = if terminal_fd.is_some() {
            Some(signal(SignalKind::window_change())?)
        } else {
            None
        };
        // Subscribe before sampling the size so a startup resize cannot be
        // missed. Output terminals still supply dimensions with piped stdin.
        let terminal = if command.tty {
            Some(TerminalSpec {
                size: match terminal_fd {
                    Some(fd) => TerminalSize::from_fd(fd)?,
                    None => TerminalSize { rows: 24, cols: 80 },
                },
                term: match std::env::var("TERM") {
                    Ok(term) => Some(term),
                    Err(VarError::NotPresent) => None,
                    Err(error @ VarError::NotUnicode(_)) => return Err(error.into()),
                },
            })
        } else {
            None
        };
        let request = ExecRequest {
            connection: ConnectPodRequest {
                pod_name: command.name.clone(),
                repo_path: repo_path.clone(),
                client_context: ClientContext::current(),
            },
            spec: ExecSpec {
                id: ExecutionId(rand::random()),
                command: command.command.clone(),
                relative_workdir,
                terminal,
            },
        };
        let initial = exec_stream::encode_request(&request)?;
        let client = exec_stream::client_builder()
            .unix_socket(socket.as_path())
            .build()?;
        let (tx, rx) = mpsc::channel::<io::Result<Vec<u8>>>(4);
        tx.send(Ok(initial)).await.context("sending exec request")?;

        let response = client
            .post("http://localhost/pod/exec")
            .header("content-type", CONTENT_TYPE)
            .body(reqwest::Body::wrap_stream(ReceiverStream::new(rx)))
            .send()
            .await
            .context(UNKNOWN_OUTCOME)?;
        // Only a missing endpoint proves that the command was never started.
        // Retrying other failures could execute a command twice.
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            let status = response.status();
            let error = response.text().await.context(UNKNOWN_OUTCOME)?;
            return Err(anyhow::anyhow!("exec failed ({status}): {error}"));
        }
        // Leave stdin untouched until the server accepts the execution so an
        // older pod can receive the original input through enter's transport.
        let _stdin = AbortOnDropHandle::new(tokio::spawn(async move {
            if let Err(error) = forward_stdin(&tx).await {
                if let Err(send_error) = tx.send(Err(error)).await {
                    log::debug!("exec input closed after stdin error: {send_error}");
                }
            }
        }));
        // Tokio's handlers outlive their streams. Keep native signal handling
        // until acceptance so the enter fallback retains its usual behavior.
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        // Keep local Ctrl-C available during startup. Switch before displaying
        // any prompt so password input cannot be echoed by the local terminal.
        let _terminal = if interactive {
            Some(RawTerminal::new()?)
        } else {
            None
        };
        // Headers acknowledge that the execution ID has been registered. A
        // separate, bodyless HTTP/2 request bypasses both stream and connection
        // DATA flow control when the command stops consuming stdin.
        let signals = async {
            loop {
                let signal = tokio::select! {
                    _ = interrupt.recv() => ExecSignal::Interrupt,
                    _ = terminate.recv() => ExecSignal::Terminate,
                    _ = hangup.recv() => ExecSignal::Hangup,
                };
                let response = client
                    .post("http://localhost/pod/exec/signal")
                    .query(&ExecSignalRequest {
                        repo_path: request.connection.repo_path.clone(),
                        pod_name: request.connection.pod_name.clone(),
                        id: request.spec.id,
                        signal,
                    })
                    .send()
                    .await
                    .context("sending exec signal")?;
                if !response.status().is_success() {
                    let error = response.text().await?;
                    return Err(anyhow::anyhow!("forwarding exec signal: {error}"));
                }
            }
        };
        let resizes = async {
            let (Some(fd), Some(mut changes)) = (terminal_fd, window_change) else {
                return std::future::pending().await;
            };
            loop {
                changes
                    .recv()
                    .await
                    .context("exec resize signal stream closed")?;
                let size = TerminalSize::from_fd(fd)?;
                // Resizes must bypass a full input stream, including after
                // stdin EOF, so a paused TUI can still redraw at the new size.
                let response = client
                    .post("http://localhost/pod/exec/resize")
                    .query(&ExecResizeRequest {
                        repo_path: request.connection.repo_path.clone(),
                        pod_name: request.connection.pod_name.clone(),
                        id: request.spec.id,
                        rows: size.rows,
                        cols: size.cols,
                    })
                    .send()
                    .await
                    .context("sending exec resize")?;
                if !response.status().is_success() {
                    let error = response.text().await?;
                    return Err(anyhow::anyhow!("resizing exec terminal: {error}"));
                }
            }
        };
        let result = tokio::select! {
            result = read_output(response) => result,
            result = signals => result,
            result = resizes => result,
        };
        result.map(Some)
    })?;
    if let Some(code) = exit_code {
        return Ok(code);
    }

    let connection = DaemonClient::new_unix(&socket)
        .prepared_pod_connection(ConnectPodRequest {
            pod_name: command.name.clone(),
            repo_path: repo_path.clone(),
            client_context: ClientContext::current(),
        })?
        .context("pod is no longer connected for exec fallback")?;
    let status = crate::enter::enter_prepared(
        &connection,
        &repo_path,
        &command.command,
        command.tty,
        false,
    )?;
    let code = match (status.code(), status.signal()) {
        (Some(code), None) => code,
        (None, Some(signal)) => 128 + signal,
        (code, signal) => {
            return Err(anyhow::anyhow!(
                "unexpected exec status: {code:?}, {signal:?}"
            ));
        }
    };
    u8::try_from(code).context("exec exit status is outside 0..=255")
}

async fn read_output(response: reqwest::Response) -> Result<u8> {
    let mut reader = StreamReader::new(response.bytes_stream().map_err(exec_stream::io_error));
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    loop {
        match exec_stream::read_frame(&mut reader)
            .await
            .context(UNKNOWN_OUTCOME)?
        {
            Some(Frame::Stdout(data)) => {
                stdout.write_all(&data).await?;
                stdout.flush().await?;
            }
            Some(Frame::Stderr(data)) => {
                stderr.write_all(&data).await?;
                stderr.flush().await?;
            }
            Some(Frame::Exit(code)) => return Ok(code),
            Some(Frame::Error(error)) => return Err(anyhow::anyhow!("{error}")),
            Some(Frame::Stdin(_) | Frame::StdinEof) => {
                return Err(anyhow::anyhow!("invalid exec response; {UNKNOWN_OUTCOME}"));
            }
            None => return Err(anyhow::anyhow!("{UNKNOWN_OUTCOME}")),
        }
    }
}

async fn forward_stdin(tx: &mpsc::Sender<io::Result<Vec<u8>>>) -> io::Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buffer = vec![0; CHUNK_SIZE];
    loop {
        let count = stdin.read(&mut buffer).await?;
        let frame = if count == 0 {
            Frame::StdinEof
        } else {
            Frame::Stdin(buffer[..count].to_vec())
        };
        if tx.send(Ok(frame.encode())).await.is_err() || count == 0 {
            return Ok(());
        }
    }
}

pub(crate) fn daemon_routes(daemon: Arc<DaemonServer>) -> Router {
    Router::new()
        .route("/pod/exec", post(daemon_exec))
        .route("/pod/exec/signal", post(daemon_signal))
        .route("/pod/exec/resize", post(daemon_resize))
        .with_state(daemon)
}

async fn daemon_exec(
    State(daemon): State<Arc<DaemonServer>>,
    body: Body,
) -> Result<Response, (StatusCode, String)> {
    let mut reader = StreamReader::new(body.into_data_stream().map_err(exec_stream::io_error));
    let request: ExecRequest = exec_stream::read_request(&mut reader)
        .await
        .map_err(|error| (StatusCode::BAD_REQUEST, format!("{error:#}")))?;
    let (client, endpoint, http_version) = daemon
        .exec_connection(&request.connection)
        .map_err(|error| (StatusCode::CONFLICT, format!("{error:#}")))?;
    match http_version {
        reqwest::Version::HTTP_10 | reqwest::Version::HTTP_11 => {
            // Streaming exec needs HTTP/2; reject before a command can start.
            return Err((StatusCode::NOT_FOUND, "pod requires legacy exec".into()));
        }
        reqwest::Version::HTTP_2 => {}
        version => {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("unsupported pod HTTP version: {version:?}"),
            ));
        }
    }
    let initial = exec_stream::encode_request(&request.spec)
        .map_err(|error| (StatusCode::BAD_REQUEST, format!("{error:#}")))?;
    let input = futures_util::stream::once(async { Ok::<_, io::Error>(initial.into()) })
        .chain(ReaderStream::new(reader));
    let response = client
        .post(format!("{}/exec", endpoint.url))
        .bearer_auth(&endpoint.token)
        .header("content-type", CONTENT_TYPE)
        .body(reqwest::Body::wrap_stream(input))
        .send()
        .await
        .map_err(|error| {
            (
                StatusCode::BAD_GATEWAY,
                format!("{UNKNOWN_OUTCOME}: {error}"),
            )
        })?;
    Ok(Response::builder()
        .status(response.status())
        .header("content-type", CONTENT_TYPE)
        .body(Body::from_stream(response.bytes_stream()))
        .expect("valid exec response"))
}

#[derive(Serialize, Deserialize)]
struct ExecSignalRequest {
    repo_path: PathBuf,
    pod_name: String,
    id: ExecutionId,
    signal: ExecSignal,
}

#[derive(Serialize, Deserialize)]
struct ExecResizeRequest {
    repo_path: PathBuf,
    pod_name: String,
    id: ExecutionId,
    rows: u16,
    cols: u16,
}

async fn daemon_resize(
    State(daemon): State<Arc<DaemonServer>>,
    Query(request): Query<ExecResizeRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let (client, endpoint, _) = daemon
        .exec_connection(&ConnectPodRequest {
            repo_path: request.repo_path,
            pod_name: request.pod_name,
            client_context: ClientContext::default(),
        })
        .map_err(|error| (StatusCode::CONFLICT, format!("{error:#}")))?;
    let response = client
        .post(format!("{}/exec/resize", endpoint.url))
        .bearer_auth(&endpoint.token)
        .query(&ResizeRequest {
            id: request.id,
            rows: request.rows,
            cols: request.cols,
        })
        .send()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let message = response
            .text()
            .await
            .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))?;
        return Err((status, message));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn daemon_signal(
    State(daemon): State<Arc<DaemonServer>>,
    Query(request): Query<ExecSignalRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let (client, endpoint, _) = daemon
        .exec_connection(&ConnectPodRequest {
            repo_path: request.repo_path,
            pod_name: request.pod_name,
            client_context: ClientContext::default(),
        })
        .map_err(|error| (StatusCode::CONFLICT, format!("{error:#}")))?;
    let response = client
        .post(format!("{}/exec/signal", endpoint.url))
        .bearer_auth(&endpoint.token)
        .query(&SignalRequest {
            id: request.id,
            signal: request.signal,
        })
        .send()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let message = response
            .text()
            .await
            .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))?;
        return Err((status, message));
    }
    Ok(StatusCode::NO_CONTENT)
}
