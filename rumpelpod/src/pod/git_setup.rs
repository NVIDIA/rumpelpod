// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Git repository preparation logic for containers.
//!
//! Sets up remotes, hooks, branches, submodules, and identity so that
//! the pod can push/fetch through the gateway.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::command_ext::CommandExt;
use crate::git::GitIdentity;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct GitSetupRequest {
    pub repo_path: PathBuf,
    pub url: String,
    pub token: String,
    pub pod_name: String,
    /// Extra refspecs appended to `git fetch host`.  Used by `rumpel
    /// fork` to pull source-pod branches into a `source-pod/<branch>`
    /// namespace before checking out forked branches.
    pub extra_host_fetch: Vec<String>,
    /// Branches to create on first entry.  Each is created from `base`
    /// and (optionally) tracked against `upstream`.
    pub branches: Vec<GitSetupBranch>,
    /// The branch to check out and write into `git config
    /// rumpelpod.pod-name`.  Must appear in `branches` or already exist.
    pub primary: String,
    /// Git user identity from the host to write into the pod's .git/config.
    pub git_identity: Option<GitIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSetupBranch {
    pub name: String,
    /// Any ref reachable after the host fetch (e.g. "host/HEAD",
    /// "host/master", "source-pod/main").
    pub base: String,
    /// Upstream to set on the new branch (e.g. "host/master",
    /// "rumpelpod/foo@main").  None leaves the branch with no upstream.
    pub upstream: Option<String>,
}

#[derive(Debug)]
pub(crate) struct GitSetupSubmodulesRequest {
    pub repo_path: PathBuf,
    pub base_url: String,
    pub token: String,
    pub pod_name: String,
    pub is_first_entry: bool,
}

#[derive(Debug)]
pub(crate) struct GitGatewayRefreshRequest {
    pub repo_path: PathBuf,
    pub base_url: String,
    pub token: String,
}

// ---------------------------------------------------------------------------
// Hook constants
// ---------------------------------------------------------------------------

/// Hook content that delegates to the rumpel binary inside the container.
const POD_REFERENCE_TRANSACTION_HOOK: &str = "\
#!/bin/sh\n\
# Installed by rumpelpod (pod)\n\
exec /opt/rumpelpod/bin/rumpel git-hook reference-transaction \"$@\"\n";

const HOOK_SIGNATURE: &str = "Installed by rumpelpod (pod)";

// ---------------------------------------------------------------------------
// Git setup
// ---------------------------------------------------------------------------

pub fn setup_git_impl(req: &GitSetupRequest) -> Result<()> {
    let repo_path = &req.repo_path;
    let pod_name = &req.pod_name;
    let token = &req.token;
    let push_refspec = format!("+refs/heads/*:refs/rumpelpod/*@{pod_name}");
    let repo_url = &req.url;

    // Set up gateway remotes. `host` fetches branches directly from
    // the host repo; `rumpelpod` pushes pod branches back through the
    // same gateway endpoint.
    configure_gateway_urls(repo_path, repo_url, token)?;
    // Clear existing fetch refspecs (may be multi-valued from a prior
    // entry) and set the two we need.
    let _ = Command::new("git")
        .args(["config", "--unset-all", "remote.host.fetch"])
        .current_dir(repo_path)
        .success();
    Command::new("git")
        .args([
            "config",
            "--add",
            "remote.host.fetch",
            "+refs/heads/*:refs/remotes/host/*",
        ])
        .current_dir(repo_path)
        .success()?;
    // Also fetch refs/rumpelpod/host-head so we can resolve host/HEAD
    // even when the host is in detached-HEAD state.
    Command::new("git")
        .args([
            "config",
            "--add",
            "remote.host.fetch",
            "+refs/rumpelpod/host-head:refs/remotes/host/HEAD",
        ])
        .current_dir(repo_path)
        .success()?;
    // Caller-supplied extra refspecs (e.g. `rumpel fork` adds the
    // source pod's branch namespace).
    for refspec in &req.extra_host_fetch {
        Command::new("git")
            .args(["config", "--add", "remote.host.fetch", refspec])
            .current_dir(repo_path)
            .success()?;
    }
    Command::new("git")
        .args(["config", "remote.host.pushurl", "PUSH_DISABLED"])
        .current_dir(repo_path)
        .success()?;

    // Push refspecs: all branches go to refs/rumpelpod/<branch>@<pod>,
    // and the primary branch also goes to refs/rumpelpod/<pod> as a
    // shortcut so the host sees a clean rumpelpod/<pod> remote ref.
    let primary = &req.primary;
    let _ = Command::new("git")
        .args(["config", "--unset-all", "remote.rumpelpod.push"])
        .current_dir(repo_path)
        .success();
    Command::new("git")
        .args(["config", "--add", "remote.rumpelpod.push", &push_refspec])
        .current_dir(repo_path)
        .success()?;
    let primary_push = format!("+refs/heads/{primary}:refs/rumpelpod/{pod_name}");
    Command::new("git")
        .args(["config", "--add", "remote.rumpelpod.push", &primary_push])
        .current_dir(repo_path)
        .success()?;
    Command::new("git")
        .args([
            "config",
            "remote.rumpelpod.fetch",
            "+refs/rumpelpod/*:refs/remotes/rumpelpod/*",
        ])
        .current_dir(repo_path)
        .success()?;

    // Store pod name so the reference-transaction hook can push the
    // primary branch shortcut.
    Command::new("git")
        .args(["config", "rumpelpod.pod-name", primary])
        .current_dir(repo_path)
        .success()?;

    // Fetch from host (pulls extra_host_fetch refspecs too).
    Command::new("git")
        .args(["fetch", "host"])
        .current_dir(repo_path)
        .success()?;

    // Install reference-transaction hook; detect first entry from return value
    let is_first_entry = install_hook_impl(repo_path)?;

    if is_first_entry {
        // Detach HEAD before mutating branches: forks can rewrite a
        // branch (e.g. "master") that the baked image happens to have
        // checked out, and `git branch -f` refuses to touch a branch
        // currently in use by a worktree.  Skip when HEAD is unborn
        // (no commits yet) -- nothing is checked out to conflict with.
        let has_commit = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .current_dir(repo_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if has_commit {
            Command::new("git")
                .args(["checkout", "--detach", "HEAD"])
                .current_dir(repo_path)
                .success()
                .context("detaching HEAD before branch setup")?;
        }

        for branch in &req.branches {
            create_or_reset_branch(repo_path, branch)?;
        }

        Command::new("git")
            .args(["checkout", primary])
            .current_dir(repo_path)
            .success()
            .with_context(|| format!("checking out primary branch '{primary}'"))?;
    }

    // Write host git identity into the pod's .git/config
    if let Some(ref identity) = req.git_identity {
        if let Some(ref name) = identity.name {
            Command::new("git")
                .args(["config", "user.name", name])
                .current_dir(repo_path)
                .success()?;
        }
        if let Some(ref email) = identity.email {
            Command::new("git")
                .args(["config", "user.email", email])
                .current_dir(repo_path)
                .success()?;
        }
    }

    Ok(())
}

pub(crate) fn refresh_gateway_urls_impl(req: &GitGatewayRefreshRequest) -> Result<()> {
    let repo_url = format!("{}/rumpelpod.git", req.base_url);
    configure_gateway_urls(&req.repo_path, &repo_url, &req.token)?;
    refresh_submodule_gateway_urls(&req.repo_path, &req.base_url, &req.token)?;
    Ok(())
}

fn configure_gateway_urls(repo_path: &Path, repo_url: &str, token: &str) -> Result<()> {
    Command::new("git")
        .args([
            "config",
            "http.extraHeader",
            &format!("Authorization: Bearer {token}"),
        ])
        .current_dir(repo_path)
        .success()?;
    set_remote_url(repo_path, "host", repo_url)?;
    set_remote_url(repo_path, "rumpelpod", repo_url)?;
    Ok(())
}

fn set_remote_url(repo_path: &Path, remote: &str, url: &str) -> Result<()> {
    if Command::new("git")
        .args(["remote", "add", remote, url])
        .current_dir(repo_path)
        .success()
        .is_err()
    {
        Command::new("git")
            .args(["remote", "set-url", remote, url])
            .current_dir(repo_path)
            .success()?;
    }
    Ok(())
}

pub fn needs_sanitize_impl(repo_path: &Path) -> Result<bool> {
    if git_operation_in_progress(repo_path)? {
        return Ok(true);
    }

    let has_head = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .current_dir(repo_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_head {
        return Ok(true);
    }

    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(repo_path)
        .output()
        .context("checking repository status before sanitize")?;
    if !status.status.success() {
        return Ok(true);
    }

    Ok(!status.stdout.is_empty())
}

fn git_operation_in_progress(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(repo_path)
        .output()
        .context("resolving git directory before sanitize")?;
    if !output.status.success() {
        return Ok(true);
    }

    let git_dir = String::from_utf8(output.stdout).context("git directory path was not UTF-8")?;
    let git_dir = git_dir.trim();
    let git_dir = Path::new(git_dir);
    let git_dir = if git_dir.is_absolute() {
        git_dir.to_path_buf()
    } else {
        repo_path.join(git_dir)
    };

    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ] {
        if git_dir.join(marker).exists() {
            return Ok(true);
        }
    }

    Ok(false)
}

// ---------------------------------------------------------------------------
// Hook installation
// ---------------------------------------------------------------------------

/// Install the reference-transaction hook. Returns true on first install.
///
/// Strips any host-side hook lines first (they reference binaries that
/// do not exist in the container), then appends the pod hook.
fn install_hook_impl(repo_path: &Path) -> Result<bool> {
    let hooks_dir = repo_path.join(".git/hooks");
    let hooks_dir_display = hooks_dir.display();
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating hooks dir {hooks_dir_display}"))?;

    let hook_path = hooks_dir.join("reference-transaction");

    let existing = std::fs::read_to_string(&hook_path).ok();

    let final_hook = match existing {
        Some(ref content) if content.contains(HOOK_SIGNATURE) => {
            return Ok(false);
        }
        Some(ref content) => {
            let cleaned = crate::gateway::strip_host_hooks(content);
            let trimmed = cleaned.trim_end();
            format!("{trimmed}\n\n{POD_REFERENCE_TRANSACTION_HOOK}")
        }
        None => POD_REFERENCE_TRANSACTION_HOOK.to_string(),
    };

    let hook_path_display = hook_path.display();
    std::fs::write(&hook_path, &final_hook)
        .with_context(|| format!("writing hook {hook_path_display}"))?;

    let mut perms = std::fs::metadata(&hook_path)
        .context("reading hook metadata")?
        .permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(&hook_path, perms).context("chmod +x hook")?;

    Ok(true)
}

/// Create (or reset) a local branch at `base` and optionally set upstream.
fn create_or_reset_branch(repo_path: &Path, branch: &GitSetupBranch) -> Result<()> {
    let name = &branch.name;
    let base = &branch.base;

    let branch_exists = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ])
        .current_dir(repo_path)
        .success()
        .is_ok();

    if branch_exists {
        Command::new("git")
            .args(["branch", "-f", "--no-track", name, base])
            .current_dir(repo_path)
            .success()
            .with_context(|| format!("resetting branch '{name}' to '{base}'"))?;
    } else {
        Command::new("git")
            .args(["branch", "--no-track", name, base])
            .current_dir(repo_path)
            .success()
            .with_context(|| format!("creating branch '{name}' from '{base}'"))?;
    }

    if let Some(ref upstream) = branch.upstream {
        Command::new("git")
            .args(["branch", "--set-upstream-to", upstream, name])
            .current_dir(repo_path)
            .success()
            .with_context(|| format!("setting upstream of '{name}' to '{upstream}'"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Submodule types and detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SubmoduleEntry {
    name: String,
    path: String,
    displaypath: String,
}

/// Detect submodules by parsing .gitmodules, recursing into nested ones
/// after they are cloned.  Returns entries sorted parents-before-children.
fn detect_submodules_from_gitmodules(repo_path: &Path, prefix: &str) -> Vec<SubmoduleEntry> {
    let gitmodules_path = repo_path.join(".gitmodules");
    if !gitmodules_path.exists() {
        return Vec::new();
    }
    let output = match Command::new("git")
        .args([
            "config",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"submodule\..*\.path",
        ])
        .current_dir(repo_path)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let mut subs = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Lines look like: submodule.foo.path libs/foo
        let mut parts = line.splitn(2, ' ');
        let key = match parts.next() {
            Some(k) => k,
            None => continue,
        };
        let path = match parts.next() {
            Some(p) => p.to_string(),
            None => continue,
        };
        // Extract name from "submodule.<name>.path"
        let name = key
            .strip_prefix("submodule.")
            .and_then(|s| s.strip_suffix(".path"))
            .unwrap_or(&path)
            .to_string();
        let displaypath = if prefix.is_empty() {
            path.clone()
        } else {
            format!("{prefix}/{path}")
        };
        subs.push(SubmoduleEntry {
            name,
            path,
            displaypath,
        });
    }
    subs
}

fn detect_existing_submodules_recursive(parent_dir: &Path, prefix: &str) -> Vec<SubmoduleEntry> {
    let submodules = detect_submodules_from_gitmodules(parent_dir, prefix);
    let mut all = Vec::new();
    for sub in submodules {
        let sub_worktree = parent_dir.join(&sub.path);
        // An uninitialized submodule leaves at most an empty
        // placeholder directory in the worktree.  Git commands run
        // there resolve to the parent repo and would rewrite its
        // config, so only include submodules with a gitlink.
        if !sub_worktree.join(".git").exists() {
            continue;
        }
        all.push(sub.clone());
        all.extend(detect_existing_submodules_recursive(
            &sub_worktree,
            &sub.displaypath,
        ));
    }
    all
}

fn submodule_parent_dir(container_repo_path: &Path, sub: &SubmoduleEntry) -> PathBuf {
    if sub.displaypath == sub.path {
        return container_repo_path.to_path_buf();
    }

    let suffix = format!("/{}", sub.path);
    let parent_displaypath = sub
        .displaypath
        .strip_suffix(&suffix)
        .expect("nested submodule displaypath ends with its local path");
    container_repo_path.join(parent_displaypath)
}

fn refresh_submodule_gateway_urls(
    container_repo_path: &Path,
    base_url: &str,
    token: &str,
) -> Result<()> {
    let submodules = detect_existing_submodules_recursive(container_repo_path, "");
    for sub in &submodules {
        refresh_submodule_gateway_url(container_repo_path, sub, base_url, token)?;
    }
    Ok(())
}

fn refresh_submodule_gateway_url(
    container_repo_path: &Path,
    sub: &SubmoduleEntry,
    base_url: &str,
    token: &str,
) -> Result<()> {
    let sub_path = container_repo_path.join(&sub.displaypath);
    let displaypath = &sub.displaypath;
    let sub_url = format!("{base_url}/submodules/{displaypath}/rumpelpod.git");

    let parent_dir = submodule_parent_dir(container_repo_path, sub);
    let submodule_url_key = format!("submodule.{}.url", sub.name);
    Command::new("git")
        .args(["config", &submodule_url_key, &sub_url])
        .current_dir(&parent_dir)
        .success()
        .with_context(|| format!("updating URL for submodule '{displaypath}'"))?;

    Command::new("git")
        .args([
            "config",
            "http.extraHeader",
            &format!("Authorization: Bearer {token}"),
        ])
        .current_dir(&sub_path)
        .success()
        .with_context(|| format!("updating auth for submodule '{displaypath}'"))?;
    set_remote_url(&sub_path, "host", &sub_url)
        .with_context(|| format!("updating host remote for submodule '{displaypath}'"))?;
    set_remote_url(&sub_path, "rumpelpod", &sub_url)
        .with_context(|| format!("updating rumpelpod remote for submodule '{displaypath}'"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Submodule setup
// ---------------------------------------------------------------------------

pub fn setup_submodules_impl(req: &GitSetupSubmodulesRequest) -> Result<()> {
    let container_repo_path = &req.repo_path;

    // Detect submodules from .gitmodules in the repo.
    let submodules = detect_submodules_from_gitmodules(container_repo_path, "");
    if submodules.is_empty() {
        return Ok(());
    }

    // Clone submodules on first entry, then recurse for nested ones.
    if req.is_first_entry {
        fn clone_recursive(
            parent_dir: &Path,
            prefix: &str,
            base_url: &str,
            token: &str,
        ) -> Result<Vec<SubmoduleEntry>> {
            let subs = detect_submodules_from_gitmodules(parent_dir, prefix);
            let mut all = Vec::new();
            for sub in &subs {
                let displaypath = &sub.displaypath;
                let sub_url = format!("{base_url}/submodules/{displaypath}/rumpelpod.git");

                Command::new("git")
                    .args(["submodule", "init", &sub.path])
                    .current_dir(parent_dir)
                    .success()?;
                let sub_name = &sub.name;
                let sub_config_key = format!("submodule.{sub_name}.url");
                Command::new("git")
                    .args(["config", &sub_config_key, &sub_url])
                    .current_dir(parent_dir)
                    .success()?;
                let auth_header = format!("http.extraHeader=Authorization: Bearer {token}");
                Command::new("git")
                    .args(["-c", &auth_header, "submodule", "update", &sub.path])
                    .current_dir(parent_dir)
                    .success()?;

                // Recurse into the cloned submodule for nested submodules.
                let sub_worktree = parent_dir.join(&sub.path);
                let nested = clone_recursive(&sub_worktree, displaypath, base_url, token)?;
                all.push(sub.clone());
                all.extend(nested);
            }
            Ok(all)
        }

        let all_subs = clone_recursive(container_repo_path, "", &req.base_url, &req.token)?;

        // Configure remotes, hooks, branches for all discovered submodules.
        for sub in &all_subs {
            configure_submodule(
                container_repo_path,
                sub,
                &req.base_url,
                &req.token,
                &req.pod_name,
                true,
            )?;
        }
        return Ok(());
    }

    // Re-entry: just reconfigure existing submodules.
    for sub in &submodules {
        configure_submodule(
            container_repo_path,
            sub,
            &req.base_url,
            &req.token,
            &req.pod_name,
            false,
        )?;
    }
    Ok(())
}

fn configure_submodule(
    container_repo_path: &Path,
    sub: &SubmoduleEntry,
    base_url: &str,
    token: &str,
    pod_name: &str,
    is_first_entry: bool,
) -> Result<()> {
    let sub_path = container_repo_path.join(&sub.displaypath);
    let displaypath = &sub.displaypath;
    let sub_url = format!("{base_url}/submodules/{displaypath}/rumpelpod.git");
    let push_refspec = format!("+refs/heads/*:refs/rumpelpod/*@{pod_name}");

    // Resolve the git dir (submodules use gitlink files)
    let git_dir_output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(&sub_path)
        .output()
        .context("resolving submodule git dir")?;
    let git_dir_relative = String::from_utf8_lossy(&git_dir_output.stdout)
        .trim()
        .to_string();
    let git_dir = if Path::new(&git_dir_relative).is_absolute() {
        PathBuf::from(&git_dir_relative)
    } else {
        sub_path.join(&git_dir_relative)
    };

    Command::new("git")
        .args([
            "config",
            "http.extraHeader",
            &format!("Authorization: Bearer {token}"),
        ])
        .current_dir(&sub_path)
        .success()?;

    if Command::new("git")
        .args(["remote", "add", "host", &sub_url])
        .current_dir(&sub_path)
        .success()
        .is_err()
    {
        Command::new("git")
            .args(["remote", "set-url", "host", &sub_url])
            .current_dir(&sub_path)
            .success()?;
    }
    let _ = Command::new("git")
        .args(["config", "--unset-all", "remote.host.fetch"])
        .current_dir(&sub_path)
        .success();
    Command::new("git")
        .args([
            "config",
            "--add",
            "remote.host.fetch",
            "+refs/heads/*:refs/remotes/host/*",
        ])
        .current_dir(&sub_path)
        .success()?;
    Command::new("git")
        .args([
            "config",
            "--add",
            "remote.host.fetch",
            "+refs/rumpelpod/host-head:refs/remotes/host/HEAD",
        ])
        .current_dir(&sub_path)
        .success()?;
    Command::new("git")
        .args(["config", "remote.host.pushurl", "PUSH_DISABLED"])
        .current_dir(&sub_path)
        .success()?;

    if Command::new("git")
        .args(["remote", "add", "rumpelpod", &sub_url])
        .current_dir(&sub_path)
        .success()
        .is_err()
    {
        Command::new("git")
            .args(["remote", "set-url", "rumpelpod", &sub_url])
            .current_dir(&sub_path)
            .success()?;
    }
    let _ = Command::new("git")
        .args(["config", "--unset-all", "remote.rumpelpod.push"])
        .current_dir(&sub_path)
        .success();
    Command::new("git")
        .args(["config", "--add", "remote.rumpelpod.push", &push_refspec])
        .current_dir(&sub_path)
        .success()?;
    let primary_push = format!("+refs/heads/{pod_name}:refs/rumpelpod/{pod_name}");
    Command::new("git")
        .args(["config", "--add", "remote.rumpelpod.push", &primary_push])
        .current_dir(&sub_path)
        .success()?;
    Command::new("git")
        .args([
            "config",
            "remote.rumpelpod.fetch",
            "+refs/rumpelpod/*:refs/remotes/rumpelpod/*",
        ])
        .current_dir(&sub_path)
        .success()?;

    Command::new("git")
        .args(["fetch", "host"])
        .current_dir(&sub_path)
        .success()
        .with_context(|| format!("fetching host in submodule '{displaypath}'"))?;

    // Install hook in submodule
    let hooks_dir = git_dir.join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("reference-transaction");

    let existing = std::fs::read_to_string(&hook_path).ok();
    let needs_install = existing
        .as_ref()
        .is_none_or(|c| !c.contains(HOOK_SIGNATURE));

    if needs_install {
        let content = match existing {
            Some(ref c) => {
                let cleaned = crate::gateway::strip_host_hooks(c);
                let trimmed = cleaned.trim_end();
                format!("{trimmed}\n\n{POD_REFERENCE_TRANSACTION_HOOK}")
            }
            None => POD_REFERENCE_TRANSACTION_HOOK.to_string(),
        };
        std::fs::write(&hook_path, &content)?;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&hook_path, perms)?;
    }

    if is_first_entry {
        let branch_name = pod_name;
        let branch_exists = Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch_name}"),
            ])
            .current_dir(&sub_path)
            .success()
            .is_ok();

        if branch_exists {
            Command::new("git")
                .args(["branch", "-f", "--no-track", branch_name, "host/HEAD"])
                .current_dir(&sub_path)
                .success()?;
        } else {
            Command::new("git")
                .args(["branch", "--no-track", branch_name, "host/HEAD"])
                .current_dir(&sub_path)
                .success()?;
        }
        Command::new("git")
            .args(["checkout", branch_name])
            .current_dir(&sub_path)
            .success()?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Sanitize
// ---------------------------------------------------------------------------

pub fn sanitize_impl(repo_path: &Path) -> Result<()> {
    // Abort any in-progress operations
    for op in &[
        &["merge", "--abort"][..],
        &["rebase", "--abort"],
        &["cherry-pick", "--abort"],
        &["revert", "--abort"],
        &["am", "--abort"],
        &["bisect", "reset"],
    ] {
        let _ = Command::new("git")
            .args(*op)
            .current_dir(repo_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    // Check if HEAD is valid
    let has_head = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(repo_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if has_head {
        Command::new("git")
            .args(["reset", "--hard", "HEAD"])
            .current_dir(repo_path)
            .success()?;
    } else {
        let _ = Command::new("git")
            .args(["rm", "--cached", "-r", "."])
            .current_dir(repo_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(repo_path)
        .success()?;
    Ok(())
}
