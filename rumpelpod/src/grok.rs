// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `rumpel grok`: launch the xAI Grok CLI inside a persistent PTY
//! session in a pod.
//!
//! Like `rumpel claude`, the grok TUI runs inside the container and the
//! client attaches to a screen session over the pod server's `/grok`
//! WebSocket route.  Authentication is forwarded two ways, mirroring the
//! ways grok itself accepts credentials: the host's `XAI_API_KEY` is
//! injected into the session environment, and `~/.grok/auth.json` (from
//! `grok login`) is copied into the pod when present.  `~/.grok/config.toml`
//! is copied alongside it so the user's model and CLI settings carry over.

use std::time::Instant;

use anyhow::{Context, Result};
use log::trace;

use crate::cli::GrokCommand;
use crate::config::load_json_config;
use crate::daemon;
use crate::enter::{confirm_pod_creation, launch_pod};
use crate::git::get_repo_root;
use crate::pty_attach;

const PTY_SESSION_NAME: &str = "grok";

/// Environment variable holding the xAI API key.  Forwarded from the
/// host into the in-pod session so grok can authenticate without the
/// user logging in inside the container.
const XAI_API_KEY_ENV: &str = "XAI_API_KEY";

pub fn grok(cmd: &GrokCommand) -> Result<()> {
    let t_total = Instant::now();

    let repo_root = get_repo_root()?;
    let host_override = cmd.host_args.resolve()?;
    let json_config = load_json_config(&repo_root)?;

    // CLI --no-always-approve wins over the config setting.
    let always_approve = !cmd.no_always_approve && json_config.grok.always_approve;

    confirm_pod_creation(&cmd.name, &repo_root, cmd.create)?;

    let t = Instant::now();
    let result = launch_pod(&cmd.name, host_override)?;
    trace!("launch_pod: {:?}", t.elapsed());
    let workdir = result.container_repo_path.clone();

    let pod = crate::pod::PodClient::connect(&result.container_url, &result.container_token)?;
    let mut session_env = Vec::new();
    write_grok_credentials(&pod, &mut session_env)?;

    let mut grok_cmd = vec![crate::daemon::GROK_CONTAINER_BIN.to_string()];
    if always_approve {
        grok_cmd.push("--always-approve".to_string());
    }
    grok_cmd.extend(cmd.args.clone());

    let workdir_str = workdir.to_string_lossy().to_string();
    trace!("total grok startup: {:?}", t_total.elapsed());

    let reconnect = Some(pty_attach::ReconnectConfig {
        daemon_socket: daemon::socket_path()?,
        repo_path: repo_root.clone(),
        pod_name: cmd.name.clone(),
    });

    let outcome = pty_attach::attach(
        pty_attach::PtyTransport::Tcp {
            url: result.container_url.clone(),
        },
        "/grok",
        &result.container_token,
        pty_attach::WireParams::Session(pty_attach::SessionParams {
            name: PTY_SESSION_NAME.to_string(),
            cmd: grok_cmd,
            workdir: Some(workdir_str),
            env: session_env,
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

/// Forward grok credentials and config from the local machine into the
/// pod.
///
/// Appends `XAI_API_KEY=<value>` to `session_env` when set on the host,
/// and copies `~/.grok/auth.json` (written by `grok login`) plus
/// `~/.grok/config.toml` into the pod when present.  Either the env key
/// or auth.json is sufficient to authenticate; credentials may also be
/// supplied by the devcontainer environment, so a missing local
/// credential is a warning rather than an error.
fn write_grok_credentials(
    pod: &crate::pod::PodClient,
    session_env: &mut Vec<String>,
) -> Result<()> {
    let mut have_credential = false;

    if let Some(key) = std::env::var_os(XAI_API_KEY_ENV) {
        let key = key.to_string_lossy().into_owned();
        if !key.is_empty() {
            session_env.push(format!("{XAI_API_KEY_ENV}={key}"));
            have_credential = true;
        }
    }

    let local_home = dirs::home_dir().context("could not determine home directory")?;
    let grok_dir = local_home.join(".grok");
    let auth_path = grok_dir.join("auth.json");
    let auth_present = auth_path.exists();
    have_credential |= auth_present;

    let entries: Vec<(String, std::path::PathBuf)> = vec![
        (".grok/auth.json".to_string(), auth_path),
        (
            ".grok/config.toml".to_string(),
            grok_dir.join("config.toml"),
        ),
    ]
    .into_iter()
    .filter(|(_, p)| p.exists())
    .collect();

    if !entries.is_empty() {
        let (read_end, write_end) = std::io::pipe().context("creating pipe for grok tar")?;
        let handle = std::thread::spawn(move || -> Result<()> {
            let mut archive = tar::Builder::new(write_end);
            for (rel, src) in &entries {
                archive
                    .append_path_with_name(src, rel)
                    .with_context(|| format!("archiving {rel}"))?;
            }
            archive.into_inner().context("finalizing grok tar")?;
            Ok(())
        });
        pod.put_agent_files("grok", read_end, None)
            .context("uploading grok credentials")?;
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("grok tar thread panicked"))??;
    }

    if !have_credential {
        eprintln!(
            "warning: no grok credentials found (set {XAI_API_KEY_ENV} or run `grok login`). \
             Relying on the devcontainer environment for authentication."
        );
    }

    Ok(())
}
