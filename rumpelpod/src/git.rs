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
const LFS_POINTER_MAX_BYTES: u64 = 4096;

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

/// Prepare a new destination ref for a push that skips Git LFS' pre-push hook.
///
/// Git LFS treats a missing destination ref as "upload everything reachable".
/// Pod refs are often new names for commits already fetched from the host or
/// another pod, so that full-history scan can fail on old payloads the branch
/// did not introduce.  When fetched remote-tracking refs are ancestors of the
/// new head, upload only LFS payloads outside those refs; the caller can then
/// set GIT_LFS_SKIP_PUSH for the Git ref update.
pub(crate) fn prepare_lfs_for_new_ref(
    repo_path: &Path,
    remote: &str,
    newvalue: &str,
) -> Result<bool> {
    if newvalue == ZERO_OID || !git_lfs_available(repo_path)? {
        return Ok(false);
    }

    let bases = remote_tracking_ancestors(repo_path, newvalue)?;
    if bases.is_empty() {
        return Ok(false);
    }
    let oids = lfs_oids_for_ref_update(repo_path, &bases, newvalue)?;
    upload_lfs_oids(repo_path, remote, &oids)?;
    Ok(true)
}

/// Prepare a wildcard rumpelpod push when it will create destination refs.
///
/// A plain `git push rumpelpod` may update existing refs and create new ones in
/// the same batch.  Return true only when every new destination ref has
/// fetched bases we can compare against; the caller can then skip Git LFS'
/// full-history pre-push scan for the batch.
pub(crate) fn prepare_lfs_for_rumpelpod_push(repo_path: &Path, pod_name: &str) -> Result<bool> {
    if !git_lfs_available(repo_path)? {
        return Ok(false);
    }

    let primary = primary_branch(repo_path)?;
    let mut prepared_new_ref = false;
    for (branch, newvalue, _upstream) in local_branch_heads(repo_path)? {
        let tracking = format!("refs/remotes/rumpelpod/{branch}@{pod_name}");
        let shortcut = format!("refs/remotes/rumpelpod/{pod_name}");
        let new_namespaced_ref = rev_parse_optional(repo_path, &tracking)?.is_none();
        let new_shortcut_ref =
            branch == primary && rev_parse_optional(repo_path, &shortcut)?.is_none();
        if !new_namespaced_ref && !new_shortcut_ref {
            continue;
        }

        let bases = remote_tracking_ancestors(repo_path, &newvalue)?;
        if bases.is_empty() {
            return Ok(false);
        }
        let oids = lfs_oids_for_ref_update(repo_path, &bases, &newvalue)?;
        upload_lfs_oids(repo_path, "rumpelpod", &oids)?;
        prepared_new_ref = true;
    }

    Ok(prepared_new_ref)
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
    oldvalues: &[String],
    newvalue: &str,
) -> Result<Vec<String>> {
    if oldvalues.iter().any(|oldvalue| oldvalue == newvalue) {
        return Ok(Vec::new());
    }

    let object_ids = object_ids_in_ref_update(repo_path, oldvalues, newvalue)?;
    let blob_ids = small_blob_ids(repo_path, &object_ids)?;
    let mut oids = BTreeSet::new();
    for blob_id in blob_ids {
        if let Some(oid) = lfs_oid_from_blob(repo_path, &blob_id)? {
            oids.insert(oid);
        }
    }
    Ok(oids.into_iter().collect())
}

fn object_ids_in_ref_update(
    repo_path: &Path,
    oldvalues: &[String],
    newvalue: &str,
) -> Result<Vec<String>> {
    let mut command = Command::new("git");
    command.args(["rev-list", "--objects", newvalue]);
    if !oldvalues.is_empty() {
        command.arg("--not");
        command.args(oldvalues);
    }
    let output = command
        .current_dir(repo_path)
        .output()
        .context("listing objects in ref update")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("git rev-list failed: {stderr}"));
    }

    let listing = String::from_utf8(output.stdout).context("rev-list output was not UTF-8")?;
    let mut object_ids = Vec::new();
    for line in listing.lines() {
        let Some((object_id, _path)) = line.split_once(' ') else {
            object_ids.push(line.to_string());
            continue;
        };
        object_ids.push(object_id.to_string());
    }
    Ok(object_ids)
}

fn small_blob_ids(repo_path: &Path, object_ids: &[String]) -> Result<Vec<String>> {
    if object_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut child = Command::new("git")
        .args([
            "cat-file",
            "--batch-check=%(objectname)%09%(objecttype)%09%(objectsize)",
        ])
        .current_dir(repo_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git cat-file --batch-check")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("git cat-file stdin was not captured"))?;
        for object_id in object_ids {
            writeln!(stdin, "{object_id}").context("writing git object id")?;
        }
    }

    let output = child
        .wait_with_output()
        .context("waiting for git cat-file --batch-check")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "git cat-file --batch-check failed: {stderr}"
        ));
    }

    let listing = String::from_utf8(output.stdout).context("cat-file output was not UTF-8")?;
    let mut blob_ids = Vec::new();
    for line in listing.lines() {
        let mut parts = line.split('\t');
        let Some(object_id) = parts.next() else {
            return Err(anyhow::anyhow!(
                "git cat-file returned malformed line: {line}"
            ));
        };
        let Some(object_type) = parts.next() else {
            return Err(anyhow::anyhow!(
                "git cat-file returned malformed line: {line}"
            ));
        };
        let Some(object_size) = parts.next() else {
            return Err(anyhow::anyhow!(
                "git cat-file returned malformed line: {line}"
            ));
        };
        if parts.next().is_some() {
            return Err(anyhow::anyhow!(
                "git cat-file returned malformed line: {line}"
            ));
        }

        match object_type {
            "blob" => {
                let object_size = object_size
                    .parse::<u64>()
                    .with_context(|| format!("parsing git blob size from line: {line}"))?;
                if object_size <= LFS_POINTER_MAX_BYTES {
                    blob_ids.push(object_id.to_string());
                }
            }
            "commit" | "tree" | "tag" => {}
            other => {
                return Err(anyhow::anyhow!("unknown git object type: {other}"));
            }
        }
    }
    Ok(blob_ids)
}

fn lfs_oid_from_blob(repo_path: &Path, blob_id: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["cat-file", "-p", blob_id])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("reading git blob {blob_id}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "git cat-file -p {blob_id} failed: {stderr}"
        ));
    }
    Ok(lfs_oid_from_pointer(&output.stdout))
}

fn lfs_oid_from_pointer(content: &[u8]) -> Option<String> {
    if content.len() as u64 > LFS_POINTER_MAX_BYTES {
        return None;
    }
    let text = std::str::from_utf8(content).ok()?;
    let mut lines = text.lines();
    let version = lines.next()?.trim_end_matches('\r');
    if version != "version https://git-lfs.github.com/spec/v1" {
        return None;
    }
    for line in lines {
        let line = line.trim_end_matches('\r');
        let Some(oid) = line.strip_prefix("oid sha256:") else {
            continue;
        };
        if oid.len() == 64 && oid.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
            return Some(oid.to_string());
        }
    }
    None
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

fn primary_branch(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["config", "--get", "rumpelpod.pod-name"])
        .current_dir(repo_path)
        .output()
        .context("reading rumpelpod.pod-name")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "git config rumpelpod.pod-name failed: {stderr}"
        ));
    }
    let primary = String::from_utf8(output.stdout).context("primary branch was not UTF-8")?;
    let primary = primary.trim();
    if primary.is_empty() {
        return Err(anyhow::anyhow!("rumpelpod.pod-name is empty"));
    }
    Ok(primary.to_string())
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

fn remote_tracking_ancestors(repo_path: &Path, newvalue: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["for-each-ref", "--format=%(objectname)", "refs/remotes/"])
        .current_dir(repo_path)
        .output()
        .context("listing remote-tracking refs")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("git for-each-ref failed: {stderr}"));
    }

    let listing = String::from_utf8(output.stdout).context("remote ref listing was not UTF-8")?;
    let mut ancestors = Vec::new();
    for line in listing.lines() {
        let candidate = line.trim();
        if candidate.is_empty() || !is_ancestor(repo_path, candidate, newvalue)? {
            continue;
        }
        ancestors.push(candidate.to_string());
    }
    Ok(ancestors)
}

fn is_ancestor(repo_path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("checking whether {ancestor} is an ancestor of {descendant}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        Some(_) | None => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(anyhow::anyhow!("git merge-base failed: {stderr}"))
        }
    }
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
