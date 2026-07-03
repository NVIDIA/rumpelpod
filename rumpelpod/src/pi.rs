// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `rumpel pi`: host-side CLI that launches the pi coding agent inside a
//! pod and attaches to its PTY session.
//!
//! Like `rumpel claude`, pi runs in-pod: the prepared image installs the
//! host's pinned pi version, this command copies the local pi auth into
//! the pod, then attaches a PTY session whose child is the in-pod `pi`.
//! Unlike codex (whose TUI runs on the host), pi has no host process.

use std::time::Instant;

use anyhow::Result;
use log::trace;

use crate::cli::PiCommand;
use crate::config::load_json_config;
use crate::daemon;
use crate::daemon::protocol::{ContainerId, Daemon, DaemonClient, EnsurePiConfigRequest, PodName};
use crate::enter::{confirm_pod_creation, launch_pod};
use crate::git::get_repo_root;
use crate::pty_attach;

const PTY_SESSION_NAME: &str = "pi";

pub fn pi(cmd: &PiCommand) -> Result<()> {
    let t_total = Instant::now();

    let repo_root = get_repo_root()?;
    let host_override = cmd.host_args.resolve()?;
    let json_config = load_json_config(&repo_root)?;

    confirm_pod_creation(&cmd.name, &repo_root, cmd.create)?;

    let t = Instant::now();
    let result = launch_pod(&cmd.name, host_override)?;
    let elapsed = t.elapsed();
    trace!("launch_pod: {elapsed:?}");
    let workdir = result.container_repo_path.clone();

    // Copy pi auth/config into the container (idempotent: the daemon
    // skips the copy if it has already run for this pod).
    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);
    let pod_name = PodName::new(cmd.name.clone()).map_err(|e| anyhow::anyhow!(e))?;
    client.ensure_pi_config(EnsurePiConfigRequest {
        pod_name,
        repo_path: repo_root.clone(),
        container_id: ContainerId(result.container_id.0.clone()),
        container_url: result.container_url.clone(),
        container_token: result.container_token.clone(),
        trust_workspace: json_config.pi.trust_workspace,
    })?;

    let mut pi_cmd = vec![crate::daemon::PI_CONTAINER_BIN.to_string()];
    pi_cmd.extend(cmd.args.clone());

    let workdir_str = workdir.to_string_lossy().to_string();

    let elapsed = t_total.elapsed();
    trace!("total pi startup: {elapsed:?}");

    let reconnect = Some(pty_attach::ReconnectConfig {
        daemon_socket: daemon::socket_path()?,
        repo_path: repo_root.clone(),
        pod_name: cmd.name.clone(),
    });

    let outcome = pty_attach::attach(
        pty_attach::PtyTransport::Tcp {
            url: result.container_url.clone(),
        },
        "/pi",
        &result.container_token,
        pty_attach::WireParams::Session(pty_attach::SessionParams {
            name: PTY_SESSION_NAME.to_string(),
            cmd: pi_cmd,
            workdir: Some(workdir_str),
            env: vec![],
        }),
        reconnect,
    )?;

    match outcome {
        pty_attach::AttachOutcome::Detached => {
            eprintln!("[detached from session]");
        }
        pty_attach::AttachOutcome::SessionEnded => {}
    }

    Ok(())
}
