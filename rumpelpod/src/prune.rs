// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::{self, IsTerminal, Write};

use anyhow::Result;

use crate::cli::PruneCommand;
use crate::daemon;
use crate::daemon::protocol::{Daemon, DaemonClient, ListPodsRequest, PodName, PodStatus};
use crate::git::get_repo_root;

/// Returns true if a pod in this status should be removed by `prune`.
fn is_prunable(status: &PodStatus) -> bool {
    match status {
        PodStatus::Stopped => true,
        PodStatus::Gone => true,
        PodStatus::Broken => true,
        PodStatus::Running => false,
        PodStatus::Disconnected => false,
        PodStatus::Stopping => false,
        PodStatus::Deleting => false,
    }
}

/// Returns true if the repo_state string indicates the pod has unmerged
/// commits (i.e. is ahead of the host).
fn is_ahead(repo_state: Option<&str>) -> bool {
    match repo_state {
        Some(s) => s.contains("ahead"),
        None => false,
    }
}

pub fn prune(cmd: &PruneCommand) -> Result<()> {
    let repo_path = get_repo_root()?;

    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);

    let list_request = match cmd.force {
        true => ListPodsRequest::cached(repo_path.clone()),
        false => ListPodsRequest::synchronized(repo_path.clone()),
    };
    let pods = client.list_pods(list_request)?;

    let prunable: Vec<_> = pods.iter().filter(|p| is_prunable(&p.status)).collect();

    if prunable.is_empty() {
        eprintln!("no stopped pods to remove");
        return Ok(());
    }

    let mut deleted = 0u32;
    let mut failed = 0u32;

    for pod in &prunable {
        if !cmd.force && is_ahead(pod.repo_state.as_deref()) {
            let state = pod.repo_state.as_deref().unwrap_or("");
            if io::stdin().is_terminal() && io::stderr().is_terminal() {
                let name = &pod.name;
                eprint!("pod '{name}' has unmerged commits ({state}), delete anyway? [y/N] ");
                io::stderr().flush()?;

                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                if !answer.trim().eq_ignore_ascii_case("y") {
                    eprintln!("skipping pod '{name}'");
                    continue;
                }
            } else {
                let name = &pod.name;
                eprintln!("pod '{name}' has unmerged commits ({state}), use --force to delete");
                failed += 1;
                continue;
            }
        }

        if let Err(e) = client.delete_pod(PodName(pod.name.clone()), repo_path.clone(), true) {
            let name = &pod.name;
            eprintln!("failed to delete pod '{name}': {e}");
            failed += 1;
        } else {
            deleted += 1;
        }
    }

    eprintln!("deleted {deleted} pod(s)");

    if failed > 0 {
        return Err(anyhow::anyhow!("{failed} pod(s) could not be deleted"));
    }

    Ok(())
}
