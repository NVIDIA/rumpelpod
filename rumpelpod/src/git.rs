// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Git repository utilities.
//!
//! This module provides utilities for working with git repositories,
//! particularly for locating the repository root from any subdirectory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::Repository;
use serde::{Deserialize, Serialize};

/// Git user identity (name and email) read from the local machine's effective config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
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
