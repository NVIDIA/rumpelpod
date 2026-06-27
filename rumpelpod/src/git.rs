// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Git repository utilities.
//!
//! This module provides utilities for working with git repositories,
//! particularly for locating the repository root from any subdirectory.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use git2::Repository;
use serde::{Deserialize, Serialize};

/// Git user identity (name and email) read from the local machine's effective config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Deserialize)]
struct GitLfsLsFiles {
    files: Option<Vec<GitLfsFile>>,
}

#[derive(Debug, Deserialize)]
struct GitLfsFile {
    oid: String,
}

/// Discover the git repository root from the current working directory.
///
/// Returns the absolute path to the repository root (the directory containing `.git`).
/// Returns an error if the current directory is not inside a git repository.
pub fn get_repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let repo = Repository::discover(&cwd)
        .with_context(|| format!("not inside a git repository: {}", cwd.display()))?;

    let workdir = repo.workdir();
    match workdir {
        Some(workdir) => {
            // git2 appends a trailing separator to workdir paths; strip
            // it so the result matches what tools like claude (which
            // encodes its cwd, no trailing separator) see and what
            // users would type manually.
            let s = workdir.to_string_lossy();
            let trimmed = s.trim_end_matches(std::path::MAIN_SEPARATOR);
            Ok(PathBuf::from(trimmed))
        }
        None => {
            // Repository is bare (no working directory)
            Err(anyhow::anyhow!(
                "cannot use rumpel in a bare git repository (needs a working tree)"
            ))
        }
    }
}

/// Read the effective git user.name and user.email for a repository,
/// respecting repo-level overrides via the config cascade.
pub fn get_git_user_config(repo_path: &Path) -> GitIdentity {
    let config = Repository::open(repo_path)
        .ok()
        .and_then(|r| r.config().ok());
    let name = config.as_ref().and_then(|c| c.get_string("user.name").ok());
    let email = config
        .as_ref()
        .and_then(|c| c.get_string("user.email").ok());
    GitIdentity { name, email }
}

/// Get the current branch name from a repository path.
///
/// Returns None if HEAD is detached (not pointing to a branch).
pub fn get_current_branch(repo_path: &std::path::Path) -> Option<String> {
    let repo = Repository::open(repo_path).ok()?;
    let head = repo.head().ok()?;

    if head.is_branch() {
        head.shorthand().ok().map(|s| s.to_string())
    } else {
        None
    }
}

/// A git remote's name and fetch URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitRemote {
    pub name: String,
    pub url: String,
}

/// Read the list of remotes (name + fetch URL) from a repository.
///
/// Skips remotes that have no URL configured. Results are sorted by
/// name so the output is stable regardless of git config ordering.
pub fn get_remotes(repo_path: &Path) -> Result<Vec<GitRemote>> {
    let repo = Repository::open(repo_path).context("opening repository")?;
    let remote_names = repo.remotes().context("listing remotes")?;
    let mut remotes = Vec::new();
    for name in remote_names.iter() {
        let name = match name.context("reading remote name")? {
            Some(name) => name,
            None => continue,
        };
        let remote = repo
            .find_remote(name)
            .with_context(|| format!("reading remote '{name}'"))?;
        if let Ok(url) = remote.url() {
            remotes.push(GitRemote {
                name: name.to_string(),
                url: url.to_string(),
            });
        }
    }
    remotes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(remotes)
}

/// Upload LFS payloads introduced by a ref update before the Git ref
/// itself is pushed.
pub(crate) fn upload_lfs_objects_for_ref_update(
    repo_path: &Path,
    remote: &str,
    oldvalue: Option<&str>,
    newvalue: &str,
) -> Result<()> {
    if newvalue == ZERO_OID || !git_lfs_available(repo_path)? {
        return Ok(());
    }

    let oids = lfs_oids_for_ref_update(repo_path, oldvalue, newvalue)?;
    upload_lfs_oids(repo_path, remote, &oids)
}

/// Upload LFS payloads introduced by all local pod branches that are
/// not represented by their rumpelpod remote-tracking refs.
pub(crate) fn upload_lfs_objects_for_rumpelpod_push(
    repo_path: &Path,
    pod_name: &str,
) -> Result<()> {
    if !git_lfs_available(repo_path)? {
        return Ok(());
    }

    for (branch, newvalue, upstream) in local_branch_heads(repo_path)? {
        let tracking = format!("refs/remotes/rumpelpod/{branch}@{pod_name}");
        let oldvalue = match rev_parse_optional(repo_path, &tracking)? {
            Some(oldvalue) => Some(oldvalue),
            None => match upstream {
                Some(ref refname) => rev_parse_optional(repo_path, refname)?,
                None => rev_parse_optional(repo_path, "refs/remotes/host/HEAD")?,
            },
        };
        if oldvalue.as_deref() == Some(newvalue.as_str()) {
            continue;
        }
        let oids = lfs_oids_for_ref_update(repo_path, oldvalue.as_deref(), &newvalue)?;
        upload_lfs_oids(repo_path, "rumpelpod", &oids)?;
    }

    Ok(())
}

fn git_lfs_available(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["lfs", "version"])
        .current_dir(repo_path)
        .output()
        .context("checking git lfs availability")?;
    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("is not a git command") {
        return Ok(false);
    }

    Err(anyhow::anyhow!("git lfs version failed: {stderr}"))
}

fn lfs_oids_for_ref_update(
    repo_path: &Path,
    oldvalue: Option<&str>,
    newvalue: &str,
) -> Result<Vec<String>> {
    let mut command = Command::new("git");
    command.args(["lfs", "ls-files", "--json", "-l"]);
    match oldvalue {
        Some(oldvalue) if oldvalue != ZERO_OID && oldvalue != newvalue => {
            command.arg(oldvalue);
        }
        Some(_) | None => {}
    }
    command.arg(newvalue);
    let output = command
        .current_dir(repo_path)
        .output()
        .context("listing changed git lfs objects")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("git lfs ls-files failed: {stderr}"));
    }

    let listing: GitLfsLsFiles =
        serde_json::from_slice(&output.stdout).context("parsing git lfs ls-files output")?;
    let oids = listing
        .files
        .unwrap_or_default()
        .into_iter()
        .map(|file| file.oid)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(oids)
}

fn upload_lfs_oids(repo_path: &Path, remote: &str, oids: &[String]) -> Result<()> {
    if oids.is_empty() {
        return Ok(());
    }

    let mut child = Command::new("git")
        .args(["lfs", "push", "--object-id", remote, "--stdin"])
        .current_dir(repo_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git lfs push")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("git lfs push stdin was not captured"))?;
        for oid in oids {
            writeln!(stdin, "{oid}").context("writing git lfs object id")?;
        }
    }

    let output = child
        .wait_with_output()
        .context("waiting for git lfs push")?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow::anyhow!(
        "git lfs push --object-id failed: {stdout}{stderr}"
    ))
}

fn local_branch_heads(repo_path: &Path) -> Result<Vec<(String, String, Option<String>)>> {
    let output = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname:lstrip=2)%09%(objectname)%09%(upstream)",
            "refs/heads/",
        ])
        .current_dir(repo_path)
        .output()
        .context("listing local branches")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("git for-each-ref failed: {stderr}"));
    }

    let listing = String::from_utf8(output.stdout).context("branch listing was not UTF-8")?;
    let mut branches = Vec::new();
    for line in listing.lines() {
        let mut parts = line.splitn(3, '\t');
        let Some(branch) = parts.next() else {
            return Err(anyhow::anyhow!(
                "git for-each-ref returned malformed line: {line}"
            ));
        };
        let Some(sha) = parts.next() else {
            return Err(anyhow::anyhow!(
                "git for-each-ref returned malformed line: {line}"
            ));
        };
        let Some(upstream) = parts.next() else {
            return Err(anyhow::anyhow!(
                "git for-each-ref returned malformed line: {line}"
            ));
        };
        let upstream = if upstream.is_empty() {
            None
        } else {
            Some(upstream.to_string())
        };
        branches.push((branch.to_string(), sha.to_string(), upstream));
    }
    Ok(branches)
}

fn rev_parse_optional(repo_path: &Path, refname: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", refname])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("resolving ref {refname}"))?;
    match output.status.code() {
        Some(0) => {
            let sha = String::from_utf8(output.stdout).context("ref sha was not UTF-8")?;
            Ok(Some(sha.trim().to_string()))
        }
        Some(1) => Ok(None),
        Some(_) | None => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(anyhow::anyhow!("git rev-parse {refname} failed: {stderr}"))
        }
    }
}
