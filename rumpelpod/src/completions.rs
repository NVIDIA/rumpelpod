// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use clap::CommandFactory;
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate, ValueCandidates};
use clap_complete::env::{Bash, Elvish, EnvCompleter, Fish, Powershell, Shells, Zsh};

use crate::cli::Cli;
use crate::daemon;
use crate::daemon::protocol::{Daemon, DaemonClient};
use crate::git::get_repo_root;

/// Completer that queries the daemon for pod names in the current repo.
///
/// Fails silently (returns no candidates) when the daemon is unreachable
/// or the working directory is not inside a git repo, so shell completion
/// is never blocked by a missing daemon.
#[derive(Clone)]
pub struct PodNameCompleter;

impl PodNameCompleter {
    pub fn candidates() -> ArgValueCandidates {
        ArgValueCandidates::new(Self)
    }
}

impl ValueCandidates for PodNameCompleter {
    fn candidates(&self) -> Vec<CompletionCandidate> {
        pod_names()
            .unwrap_or_default()
            .into_iter()
            .map(CompletionCandidate::new)
            .collect()
    }
}

/// Default daemon timeout for the completion query.  100ms is plenty
/// for a local Unix-socket roundtrip and keeps the worst-case shell
/// stall imperceptible if the daemon is wedged.
const DEFAULT_TIMEOUT_MS: u64 = 100;

/// Env var that overrides [`DEFAULT_TIMEOUT_MS`].  Exists so the PTY
/// completion test can give itself slack under parallel test load,
/// without raising the production timeout where a stalled daemon
/// must never block the user's prompt.
const TIMEOUT_ENV: &str = "RUMPELPOD_COMPLETIONS_TIMEOUT_MS";

struct RumpelBash;

impl EnvCompleter for RumpelBash {
    fn name(&self) -> &'static str {
        Bash.name()
    }

    fn is(&self, name: &str) -> bool {
        Bash.is(name)
    }

    fn write_registration(
        &self,
        var: &str,
        name: &str,
        bin: &str,
        completer: &str,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        Bash.write_registration(var, name, bin, completer, buf)
    }

    fn write_complete(
        &self,
        cmd: &mut clap::Command,
        args: Vec<OsString>,
        current_dir: Option<&Path>,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        let index: usize = std::env::var("_CLAP_COMPLETE_INDEX")
            .ok()
            .and_then(|i| i.parse().ok())
            .unwrap_or_default();
        let ifs: Option<String> = std::env::var("_CLAP_IFS").ok().and_then(|i| i.parse().ok());

        let (args, index) = merge_bash_colon_words(args, index);
        let completions = clap_complete::engine::complete(cmd, args, index, current_dir)?;

        for (i, candidate) in completions.iter().enumerate() {
            if i != 0 {
                write!(buf, "{}", ifs.as_deref().unwrap_or("\n"))?;
            }
            write!(buf, "{}", candidate.get_value().to_string_lossy())?;
        }
        Ok(())
    }
}

/// Bash splits ':' before invoking programmable completions, but cp needs
/// POD:PATH to count as one operand so clap can reach the local path slot.
fn merge_bash_colon_words(args: Vec<OsString>, index: usize) -> (Vec<OsString>, usize) {
    if args.get(1).is_none_or(|arg| arg != "cp") {
        return (args, index);
    }

    let mut merged = Vec::with_capacity(args.len());
    let mut merged_index = index;
    let mut old_index = 0;
    while old_index < args.len() {
        if old_index > 1 && old_index + 1 < args.len() && args[old_index] == ":" {
            let Some(previous) = merged.last_mut() else {
                merged.push(args[old_index].clone());
                old_index += 1;
                continue;
            };
            previous.push(":");
            previous.push(&args[old_index + 1]);
            let joined_index = merged.len() - 1;
            if index == old_index || index == old_index + 1 {
                merged_index = joined_index;
            } else if index > old_index + 1 {
                merged_index = merged_index.saturating_sub(2);
            }
            old_index += 2;
        } else {
            merged.push(args[old_index].clone());
            old_index += 1;
        }
    }

    (merged, merged_index)
}

/// Query the daemon for pod names, returning None on any error.
fn pod_names() -> Option<Vec<String>> {
    let repo_path = get_repo_root().ok()?;
    let socket_path = daemon::socket_path().ok()?;

    let timeout_ms = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let client =
        DaemonClient::new_unix_with_timeout(&socket_path, Some(Duration::from_millis(timeout_ms)));
    let pods = client.list_pods(repo_path, true, false).ok()?;
    Some(pods.into_iter().map(|p| p.name).collect())
}

/// Generate the shell completion registration script by delegating to
/// CompleteEnv.  Sets the COMPLETE env var and lets clap_complete
/// print the script and exit.
pub fn generate(shell: &str) {
    std::env::set_var("COMPLETE", shell);
    complete();
}

pub fn complete() {
    let bash = RumpelBash;
    let shells: [&dyn EnvCompleter; 5] = [&bash, &Elvish, &Fish, &Powershell, &Zsh];
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .shells(Shells(&shells))
        .complete();
}
