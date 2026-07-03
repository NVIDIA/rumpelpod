// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;

use crate::cli::StopCommand;
use crate::daemon;
use crate::daemon::protocol::{Daemon, DaemonClient, PodName};
use crate::git::get_repo_root;

pub fn stop(cmd: &StopCommand) -> Result<()> {
    let repo_path = get_repo_root()?;

    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);

    let pods = client.list_pods(repo_path.clone(), true, false)?;

    let mut failed = 0u32;

    for name in &cmd.names {
        if !pods.iter().any(|p| p.name == *name) {
            eprintln!("pod '{name}' not found");
            failed += 1;
            continue;
        }

        let pod_name = match PodName::new(name.clone()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                failed += 1;
                continue;
            }
        };
        if let Err(e) = client.stop_pod(pod_name, repo_path.clone(), cmd.wait) {
            eprintln!("failed to stop pod '{name}': {e}");
            failed += 1;
        }
    }

    if failed > 0 {
        return Err(anyhow::anyhow!("{failed} pod(s) could not be stopped"));
    }

    Ok(())
}
