// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use log::trace;

use crate::cli::ClaudeCommand;
use crate::config::load_json_config;
use crate::daemon;
use crate::daemon::protocol::{
    ContainerId, Daemon, DaemonClient, EnsureClaudeConfigRequest, PodName,
};
use crate::enter::{confirm_pod_creation, launch_pod};
use crate::git::get_repo_root;
use crate::pod::types::{base64_encode, HomeFileEntry};
use crate::pod::PodClient;
use crate::pty_attach;

const PTY_SESSION_NAME: &str = "claude";

pub fn claude(cmd: &ClaudeCommand) -> Result<()> {
    let t_total = Instant::now();

    let t = Instant::now();
    let repo_root = get_repo_root()?;
    let elapsed = t.elapsed();
    trace!("get_repo_root: {elapsed:?}");

    let host_override = cmd.host_args.resolve()?;

    let json_config = load_json_config(&repo_root)?;

    // CLI --no-dangerously-skip-permissions wins over the config setting.
    // CLI --dangerously-skip-permissions-hook forces the hook variant on,
    // implying yolo is enabled even if the config disabled it.
    let skip_permissions = !cmd.no_dangerously_skip_permissions
        && (cmd.dangerously_skip_permissions_hook
            || json_config.claude.dangerously_skip_permissions);
    let permission_hook = skip_permissions
        && (cmd.dangerously_skip_permissions_hook
            || json_config.claude.dangerously_skip_permissions_hook);

    confirm_pod_creation(&cmd.name, &repo_root, cmd.create)?;

    let t = Instant::now();
    let result = launch_pod(&cmd.name, host_override)?;
    let elapsed = t.elapsed();
    trace!("launch_pod: {elapsed:?}");
    let workdir = result.container_repo_path.clone();

    // Copy Claude config into the container.
    let t = Instant::now();
    let config_result = {
        let tc = Instant::now();
        let socket_path = daemon::socket_path()?;
        let client = DaemonClient::new_unix(&socket_path);
        let pod_name = PodName::new(cmd.name.clone()).map_err(|e| anyhow::anyhow!(e))?;
        let cfg_result = client.ensure_claude_config(EnsureClaudeConfigRequest {
            pod_name,
            repo_path: repo_root.clone(),
            container_repo_path: workdir.clone(),
            container_id: ContainerId(result.container_id.0.clone()),
            docker_socket: result.docker_socket.clone(),
            container_url: result.container_url.clone(),
            container_token: result.container_token.clone(),
            permission_hook,
            copy_sessions: json_config.claude.sessions.is_some(),
        });
        let elapsed = tc.elapsed();
        trace!("ensure_claude_config: {elapsed:?}");
        cfg_result
    };
    let elapsed = t.elapsed();
    trace!("config: {elapsed:?}");

    config_result?;

    let mut claude_cmd = vec![crate::daemon::CLAUDE_CONTAINER_BIN.to_string()];
    if skip_permissions && !permission_hook {
        claude_cmd.push("--dangerously-skip-permissions".to_string());
    }
    claude_cmd.extend(cmd.args.clone());

    let workdir_str = workdir.to_string_lossy().to_string();

    let elapsed = t_total.elapsed();
    trace!("total claude startup: {elapsed:?}");

    let reconnect = Some(pty_attach::ReconnectConfig {
        daemon_socket: daemon::socket_path()?,
        repo_path: repo_root.clone(),
        pod_name: cmd.name.clone(),
    });

    let outcome = pty_attach::attach(
        pty_attach::PtyTransport::Tcp {
            url: result.container_url.clone(),
        },
        "/claude",
        &result.container_token,
        pty_attach::WireParams::Session(pty_attach::SessionParams {
            name: PTY_SESSION_NAME.to_string(),
            cmd: claude_cmd,
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

/// Read the local Claude Code OAuth credentials, if any.
///
/// On Linux, Claude Code stores them in ~/.claude/.credentials.json.
/// On macOS it stores them in the login Keychain (service "Claude
/// Code-credentials") instead, and the file usually does not exist.
/// Try the file first, then fall back to the Keychain.
pub(crate) fn read_local_credentials(local_home: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(local_home.join(".claude/.credentials.json")) {
        Ok(data) => return Ok(Some(data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::from(e).context("reading ~/.claude/.credentials.json")),
    }
    Ok(read_keychain_credentials())
}

/// Read the Claude Code credentials from the macOS login Keychain.
///
/// The item's password is the same JSON document that
/// .credentials.json holds on Linux.  Any failure degrades to `None`
/// (same as a missing credentials file) so users authenticating by
/// other means (API key, env vars) are not blocked.
#[cfg(target_os = "macos")]
fn read_keychain_credentials() -> Option<Vec<u8>> {
    let output = match std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            log::warn!("could not run security(1) to read Claude Code Keychain credentials: {e}");
            return None;
        }
    };
    if !output.status.success() {
        // Exit code 44 (errSecItemNotFound) is the normal "no Keychain
        // credentials" case.
        if output.status.code() != Some(44) {
            log::warn!(
                "security(1) failed reading Claude Code Keychain credentials: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return None;
    }
    let mut data = output.stdout;
    if data.last() == Some(&b'\n') {
        data.pop();
    }
    (!data.is_empty()).then_some(data)
}

#[cfg(not(target_os = "macos"))]
fn read_keychain_credentials() -> Option<Vec<u8>> {
    None
}

/// Copy only authentication credentials from the local machine into
/// the pod.
///
/// Supports both OAuth (.credentials.json) and API keys (primaryApiKey
/// in .claude.json). Useful when local credentials have been refreshed
/// (e.g. token expiry) without needing to recreate the pod.
pub fn reauth(cmd: &ClaudeCommand) -> Result<()> {
    let repo_root = get_repo_root()?;
    let host_override = cmd.host_args.resolve()?;
    confirm_pod_creation(&cmd.name, &repo_root, cmd.create)?;
    let result = launch_pod(&cmd.name, host_override)?;
    let pod = PodClient::connect(&result.container_url, &result.container_token)?;

    let local_home = dirs::home_dir().context("could not determine home directory")?;

    let mut files: Vec<HomeFileEntry> = Vec::new();

    // OAuth tokens from .claude/.credentials.json or the macOS Keychain
    if let Some(data) = read_local_credentials(&local_home)? {
        files.push(HomeFileEntry {
            path: ".claude/.credentials.json".to_string(),
            content: base64_encode(&data),
            create_parents: true,
        });
    }

    // primaryApiKey -- read-modify-write on .claude.json
    match std::fs::read(local_home.join(".claude.json")) {
        Ok(local_data) => {
            let local_obj: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(&local_data).context("parsing ~/.claude.json")?;
            if let Some(key) = local_obj.get("primaryApiKey") {
                // Write credentials first so we can read the container's
                // .claude.json to merge the API key into it.
                let resp = pod.write_home_files(files.clone(), vec![])?;
                let container_path = Path::new(&resp.home).join(".claude.json");
                let container_data = pod.fs_read(&container_path).unwrap_or_default();
                let mut container_obj: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_slice(&container_data).unwrap_or_default();
                container_obj.insert("primaryApiKey".to_string(), key.clone());
                let updated = serde_json::to_vec_pretty(&container_obj)
                    .context("serializing updated .claude.json")?;
                files.push(HomeFileEntry {
                    path: ".claude.json".to_string(),
                    content: base64_encode(&updated),
                    create_parents: false,
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::from(e).context("reading ~/.claude.json")),
    }

    if !files.is_empty() {
        pod.write_home_files(files, vec![])?;
    }

    eprintln!("authentication credentials updated");
    Ok(())
}
