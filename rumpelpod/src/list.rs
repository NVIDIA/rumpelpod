// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::cmp::Reverse;

use anyhow::Result;
use comfy_table::presets::NOTHING;
use comfy_table::Table;

use crate::cli::ListCommand;
use crate::daemon;
use crate::daemon::protocol::{Daemon, DaemonClient, PodStatus};
use crate::git::get_repo_root;
use crate::pod::types::{ClaudeState, CodexState};

fn claude_state_str(state: Option<ClaudeState>) -> &'static str {
    match state {
        Some(ClaudeState::Processing) => "processing",
        Some(ClaudeState::WaitingForInput) => "idle",
        Some(ClaudeState::AuthError) => "auth error",
        Some(ClaudeState::Stopped) => "stopped",
        None => "",
    }
}

fn codex_state_str(state: Option<CodexState>) -> &'static str {
    match state {
        Some(CodexState::Processing) => "processing",
        Some(CodexState::Idle) => "idle",
        Some(CodexState::Error) => "error",
        None => "",
    }
}

pub fn list(cmd: &ListCommand) -> Result<()> {
    let repo_path = get_repo_root()?;

    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);

    let mut pods = client.list_pods(repo_path, cmd.sync, cmd.sync)?;
    // Running pods first, then by most recent commit on the pod's primary branch.
    pods.sort_by_key(|pod| {
        (
            pod.status != PodStatus::Running,
            Reverse(pod.last_commit_time),
        )
    });

    let show_claude = pods.iter().any(|pod| pod.claude_state.is_some());
    let show_codex = pods.iter().any(|pod| pod.codex_state.is_some());
    let show_host = pods.iter().any(|pod| pod.host != pods[0].host);

    let mut header: Vec<&str> = vec!["NAME"];
    if show_claude {
        header.push("CLAUDE");
    }
    if show_codex {
        header.push("CODEX");
    }
    header.extend(["GIT", "STATUS", "CREATED"]);
    if show_host {
        header.push("HOST");
    }
    header.push("CONTAINER ID");

    let mut table = Table::new();
    table.load_preset(NOTHING).set_header(header);

    for pod in pods {
        let status_str = match pod.status {
            PodStatus::Running => "running",
            PodStatus::Stopped => "stopped",
            PodStatus::Gone => "gone",
            PodStatus::Disconnected => "disconnected",
            PodStatus::Stopping => "stopping",
            PodStatus::Deleting => "deleting",
            PodStatus::Broken => "broken",
        };
        let repo_state = pod.repo_state.as_deref().unwrap_or("");
        let header = "CONTAINER ID";
        let container_id = pod.container_id.as_deref().unwrap_or("");
        let container_id = &container_id[..container_id.len().min(header.len())];

        let mut row = vec![pod.name];
        if show_claude {
            row.push(claude_state_str(pod.claude_state).to_string());
        }
        if show_codex {
            row.push(codex_state_str(pod.codex_state).to_string());
        }
        row.extend([repo_state.to_string(), status_str.to_string(), pod.created]);
        if show_host {
            row.push(pod.host);
        }
        row.push(container_id.to_string());
        table.add_row(row);
    }

    println!("{table}");

    Ok(())
}
