// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Pod-side adapter for the generic PTY screen-session machinery.
//!
//! Wires the `/claude`, `/grok`, and `/pi` WebSocket routes to the shared
//! `pty_session` bridge, with a cmd-transform that resolves the
//! in-container agent CLI binary path before the first spawn.

use std::path::{Path, PathBuf};

use anyhow::Result;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::response::Response;

pub use crate::pty_session::PtySessions;
use crate::pty_session::{serve_ws_session, CmdTransform};

// ---------------------------------------------------------------------------
// Claude CLI resolution
// ---------------------------------------------------------------------------

/// Return the path to the Claude CLI binary, which must be pre-installed
/// in the container image (baked in by the prepared image build).
fn find_claude_cli() -> Result<PathBuf> {
    let bin_path = Path::new(crate::daemon::CLAUDE_CONTAINER_BIN);
    if bin_path.exists() {
        return Ok(bin_path.to_path_buf());
    }

    if let Some(found) = crate::which("claude") {
        return Ok(found);
    }

    Err(anyhow::anyhow!(
        "Claude CLI not found at {} or in PATH",
        crate::daemon::CLAUDE_CONTAINER_BIN
    ))
}

/// Return the path to the Grok CLI binary, which must be pre-installed
/// in the container image (baked in by the prepared image build).
fn find_grok_cli() -> Result<PathBuf> {
    let bin_path = Path::new(crate::daemon::GROK_CONTAINER_BIN);
    if bin_path.exists() {
        return Ok(bin_path.to_path_buf());
    }

    if let Some(found) = crate::which("grok") {
        return Ok(found);
    }

    Err(anyhow::anyhow!(
        "Grok CLI not found at {} or in PATH",
        crate::daemon::GROK_CONTAINER_BIN
    ))
}

// ---------------------------------------------------------------------------
// HTTP handler
// ---------------------------------------------------------------------------

pub async fn claude_session_handler(
    ws: WebSocketUpgrade,
    State(state): State<super::server::PodServerState>,
) -> Response {
    ws.on_upgrade(move |socket| {
        let transform: CmdTransform = Box::new(|cmd: &mut Vec<String>| {
            let claude_bin = find_claude_cli()?;
            cmd[0] = claude_bin.to_string_lossy().into_owned();
            Ok(())
        });
        serve_ws_session(socket, state.pty_sessions, Some(transform))
    })
}

// ---------------------------------------------------------------------------
// Pi CLI resolution
// ---------------------------------------------------------------------------

/// Return the path to the pi CLI binary, which must be pre-installed in
/// the container image (baked in by the prepared image build).
fn find_pi_cli() -> Result<PathBuf> {
    let bin_path = Path::new(crate::daemon::PI_CONTAINER_BIN);
    if bin_path.exists() {
        return Ok(bin_path.to_path_buf());
    }

    if let Some(found) = crate::which("pi") {
        return Ok(found);
    }

    Err(anyhow::anyhow!(
        "pi CLI not found at {} or in PATH",
        crate::daemon::PI_CONTAINER_BIN
    ))
}

pub async fn pi_session_handler(
    ws: WebSocketUpgrade,
    State(state): State<super::server::PodServerState>,
) -> Response {
    ws.on_upgrade(move |socket| {
        let transform: CmdTransform = Box::new(|cmd: &mut Vec<String>| {
            let pi_bin = find_pi_cli()?;
            cmd[0] = pi_bin.to_string_lossy().into_owned();
            Ok(())
        });
        serve_ws_session(socket, state.pty_sessions, Some(transform))
    })
}

pub async fn grok_session_handler(
    ws: WebSocketUpgrade,
    State(state): State<super::server::PodServerState>,
) -> Response {
    ws.on_upgrade(move |socket| {
        let transform: CmdTransform = Box::new(|cmd: &mut Vec<String>| {
            let grok_bin = find_grok_cli()?;
            cmd[0] = grok_bin.to_string_lossy().into_owned();
            Ok(())
        });
        serve_ws_session(socket, state.pty_sessions, Some(transform))
    })
}
