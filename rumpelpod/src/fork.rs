// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `rumpel fork` -- spawn a new pod by cloning an existing one.
//!
//! The CLI is a thin wrapper: it validates the source/new names against
//! the daemon's pod list, optionally prompts on TTY when the source is
//! mid-turn, then forwards a `ForkPodRequest` to the daemon and streams
//! progress output back.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};

use crate::cli::ForkCommand;
use crate::daemon;
use crate::daemon::protocol::{Daemon, DaemonClient, ForkPodRequest, LaunchProgress};
use crate::git::get_repo_root;
use crate::image::OutputLine;

pub fn fork(cmd: &ForkCommand) -> Result<()> {
    let repo_root = get_repo_root()?;

    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);

    // Sanity-check names against the pod list before we ship the
    // request, so common mistakes (typo'd source, name collision)
    // surface as plain CLI errors instead of via the SSE stream.
    let pods = client.list_pods(repo_root.clone(), true, false)?;
    if !pods.iter().any(|p| p.name == cmd.source) {
        let source = &cmd.source;
        return Err(anyhow::anyhow!("source pod '{source}' does not exist"));
    }
    if pods.iter().any(|p| p.name == cmd.new_name) {
        let new_name = &cmd.new_name;
        return Err(anyhow::anyhow!("pod '{new_name}' already exists"));
    }

    // The daemon also enforces this, but the prompt is interactive so
    // it has to live here on the client side.  We default to "no"
    // because snapshotting mid-turn can pick up half-written claude
    // session state -- more conservative than `enter`'s default-yes.
    let allow_processing = if cmd.allow_processing {
        true
    } else {
        confirm_processing_if_needed(&pods, &cmd.source)?
    };

    let mut progress = client.fork_pod(ForkPodRequest {
        source: cmd.source.clone(),
        new_name: cmd.new_name.clone(),
        repo_path: repo_root,
        allow_processing,
    })?;
    for line in &mut progress {
        match line {
            OutputLine::Stdout(s) => println!("{s}"),
            OutputLine::Stderr(s) => eprintln!("{s}"),
        }
    }
    progress.finish()?;

    let new_name = &cmd.new_name;
    let source = &cmd.source;
    println!("pod '{new_name}' forked from '{source}'");
    Ok(())
}

/// If the source's claude or codex is mid-turn, ask the user whether to
/// proceed.  Default is "no".  When stdin/stderr are not both TTYs,
/// return false so the daemon errors out and the user has to opt in
/// explicitly with --allow-processing.
fn confirm_processing_if_needed(
    pods: &[crate::daemon::protocol::PodInfo],
    source: &str,
) -> Result<bool> {
    use crate::pod::types::{ClaudeState, CodexState};

    let pod = pods
        .iter()
        .find(|p| p.name == source)
        .with_context(|| format!("source pod '{source}' missing from list"))?;
    let claude_processing = matches!(pod.claude_state, Some(ClaudeState::Processing));
    let codex_processing = matches!(pod.codex_state, Some(CodexState::Processing));
    if !claude_processing && !codex_processing {
        return Ok(true);
    }

    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        // Daemon will error out with a clear message; the false here
        // just propagates the policy.
        return Ok(false);
    }

    let agents = match (claude_processing, codex_processing) {
        (true, true) => "claude and codex",
        (true, false) => "claude",
        (false, true) => "codex",
        (false, false) => unreachable!(),
    };
    eprint!(
        "source pod '{source}' is processing ({agents}). \
         Forking now may capture half-written session state. Proceed? [y/N] "
    );
    std::io::stderr().flush().ok();
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("reading confirmation from stdin")?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}
