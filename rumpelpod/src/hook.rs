// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::BufRead;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::{PreCommitDescriptionCommand, ReferenceTransactionCommand};
use crate::pod::server::TOKEN_FILE;
use crate::pod::types::{ClaudeState, NotifyClaudeStateRequest};
use crate::CommandExt;

// -- Claude Code hooks -------------------------------------------------------

/// Auto-approve permission dialogs so Claude Code can run tools without
/// manual confirmation.  Invoked as a Claude Code PermissionRequest hook.
pub fn claude_permission_request() -> Result<()> {
    println!(
        r#"{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{{"behavior":"allow"}}}}}}"#
    );
    Ok(())
}

/// Report Claude Code session state to the in-container pod server.
///
/// Invoked by Claude Code hooks (UserPromptSubmit, Stop, StopFailure,
/// SessionEnd) so the daemon can track what Claude is doing.
pub fn claude_notify_state(state_str: &str) -> Result<()> {
    let state = match state_str {
        "processing" => ClaudeState::Processing,
        "waiting_for_input" => ClaudeState::WaitingForInput,
        "auth_error" => ClaudeState::AuthError,
        "stopped" => ClaudeState::Stopped,
        other => return Err(anyhow::anyhow!("unknown claude state: {other}")),
    };

    let token = std::fs::read_to_string(TOKEN_FILE).context("reading pod server token")?;
    let port_path = std::path::Path::new(crate::port_file::SERVER_PORT_FILE);
    let port = crate::port_file::read_required(port_path).context("reading pod server port")?;

    let url = format!("http://127.0.0.1:{port}/claude-state");
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&NotifyClaudeStateRequest { state })
        .send()
        .context("posting claude state to pod server")?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(anyhow::anyhow!("pod server returned {status}"));
    }

    Ok(())
}

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// Read stdin lines in the git hook format: `oldvalue newvalue refname`.
fn read_ref_updates() -> Result<Vec<(String, String, String)>> {
    let stdin = std::io::stdin();
    let mut updates = Vec::new();
    for line in stdin.lock().lines() {
        let line = line?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        updates.push((
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        ));
    }
    Ok(updates)
}

/// Run a git command, logging failures to stderr.
fn run_git(args: &[&str]) {
    // The LFS payloads introduced by this update are uploaded before
    // the ref push; skip Git LFS' whole-branch pre-push scan.
    if let Err(e) = Command::new("git")
        .args(args)
        .env("GIT_LFS_SKIP_PUSH", "1")
        .success()
    {
        eprintln!("rumpelpod hook: {e:#}");
    }
}

// -- Pod repo hooks ----------------------------------------------------------

/// Pod reference-transaction: push branch updates to the host repo.
/// Runs after the ref update is committed, so failures are non-fatal.
pub fn reference_transaction(cmd: &ReferenceTransactionCommand) -> Result<()> {
    if cmd.state != "committed" {
        return Ok(());
    }

    // Read pod name for the primary branch shortcut.
    let pod_name = Command::new("git")
        .args(["config", "rumpelpod.pod-name"])
        .success()
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_string());

    for (oldvalue, newvalue, refname) in read_ref_updates()? {
        let branch = match refname.strip_prefix("refs/heads/") {
            Some(b) => b,
            None => continue,
        };

        if newvalue == ZERO_OID {
            run_git(&["push", "rumpelpod", "--delete", branch, "--quiet"]);
        } else {
            if oldvalue != ZERO_OID {
                if let Err(e) = crate::git::upload_lfs_objects_for_ref_update(
                    Path::new("."),
                    "rumpelpod",
                    Some(&oldvalue),
                    &newvalue,
                ) {
                    eprintln!("rumpelpod hook: git lfs push failed: {e:#}");
                    continue;
                }
            }
            run_git(&["push", "rumpelpod", branch, "--force", "--quiet"]);
        }

        // Push the primary branch shortcut (refs/rumpelpod/<pod>) so
        // the host sees a clean rumpelpod/<pod> remote ref.  git push
        // with multiple push refspecs only uses the first match, so
        // the hook must push the shortcut explicitly.
        if pod_name.as_deref() == Some(branch) {
            if newvalue == ZERO_OID {
                run_git(&[
                    "push",
                    "rumpelpod",
                    "--delete",
                    &format!("refs/rumpelpod/{branch}"),
                    "--quiet",
                ]);
            } else {
                run_git(&[
                    "push",
                    "rumpelpod",
                    &format!("HEAD:refs/rumpelpod/{branch}"),
                    "--force",
                    "--quiet",
                ]);
            }
        }
    }

    Ok(())
}

/// Pod pre-commit: validate that the DESCRIPTION file is staged and
/// formatted like a git commit message.  Non-zero exit aborts the
/// commit; the caller can retry with `git commit --no-verify`.
pub fn pre_commit_description(cmd: &PreCommitDescriptionCommand) -> Result<()> {
    let file = &cmd.file;

    // `:<path>` reads from the index, which at pre-commit time reflects
    // the tree that this commit will produce.
    let output = Command::new("git")
        .args(["cat-file", "-p", &format!(":{file}")])
        .output()
        .context("running git cat-file")?;

    if !output.status.success() {
        fail_description_check(&format!(
            "{file} is not staged for commit.\n\
             Commit a {file} file at the repo root, formatted like a git commit message, describing your branch."
        ));
    }

    let content =
        String::from_utf8(output.stdout).with_context(|| format!("{file} is not valid UTF-8"))?;

    check_description_format(file, &content);
    Ok(())
}

fn check_description_format(file: &str, content: &str) {
    let trimmed = content.trim_end_matches('\n');
    if trimmed.is_empty() {
        fail_description_check(&format!(
            "{file} is empty, the first line must be a short commit-message subject"
        ));
    }

    let lines: Vec<&str> = trimmed.split('\n').collect();
    let subject = lines[0];
    if subject.is_empty() {
        fail_description_check(&format!(
            "{file} first line is empty, it must contain a short commit-message subject"
        ));
    }
    let subject_len = subject.chars().count();
    if subject_len > 50 {
        fail_description_check(&format!(
            "{file} subject line is {subject_len} characters, keep it to 50 or fewer"
        ));
    }
    if lines.len() >= 2 && !lines[1].is_empty() {
        fail_description_check(&format!(
            "{file} line 2 must be blank to separate subject from body"
        ));
    }
    for (idx, line) in lines.iter().enumerate().skip(2) {
        let len = line.chars().count();
        if len > 72 {
            let lineno = idx + 1;
            fail_description_check(&format!(
                "{file} line {lineno} is {len} characters, keep body lines to 72 or fewer"
            ));
        }
    }
}

fn fail_description_check(msg: &str) -> ! {
    eprintln!("{msg}");
    eprintln!("bypass this check with `git commit --no-verify`");
    std::process::exit(1);
}

// -- Host repo hooks ---------------------------------------------------------
//
// These run inside the host repo (installed by rumpelpod) when pods push
// via the HTTP server.

/// Host pre-receive: access control.  Pods can only push to their own
/// namespace under refs/rumpelpod/.  This runs *before* refs are updated, so
/// returning an error rejects the push.
pub fn host_pre_receive() -> Result<()> {
    let pod_name = std::env::var("POD_NAME").ok();

    for (_old_oid, _new_oid, refname) in read_ref_updates()? {
        check_push_access(&refname, pod_name.as_deref())?;
    }

    Ok(())
}

fn check_push_access(refname: &str, pod_name: Option<&str>) -> Result<()> {
    match pod_name {
        Some(name) => {
            // Pod can push to refs/rumpelpod/<anything>@<name> and
            // refs/rumpelpod/<name> (primary branch shortcut).
            let is_namespaced =
                refname.starts_with("refs/rumpelpod/") && refname.ends_with(&format!("@{name}"));
            let is_primary_shortcut = refname == format!("refs/rumpelpod/{name}");
            if !is_namespaced && !is_primary_shortcut {
                eprintln!("error: pod '{name}' cannot push to '{refname}'");
                eprintln!(
                    "error: pods can only push to refs/rumpelpod/*@{name} or refs/rumpelpod/{name}"
                );
                return Err(anyhow::anyhow!("access denied"));
            }
        }
        None => {
            // No POD_NAME means the push did not come through the HTTP
            // server with a bearer token.  Reject everything -- the host
            // never pushes to itself via receive-pack.
            eprintln!("error: push without POD_NAME is not allowed");
            eprintln!("error: attempted to push to '{refname}'");
            return Err(anyhow::anyhow!("access denied"));
        }
    }
    Ok(())
}
