// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Review plan used by the unpublished VS Code extension.
//!
//! The `rumpel review` CLI is gone; prefer `git diff ...rumpelpod/<pod>`
//! or `git difftool ...rumpelpod/<pod>`. This module remains only because
//! that extension still POSTs `/review`. Remove it once the client diffs
//! the pod ref itself.

use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One path shown by a pod review.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "ReviewFile.ts")]
pub struct ReviewFile {
    pub path: String,
    pub base_exists: bool,
    pub target_exists: bool,
}

/// Revisions and files required to reproduce a pod review in an editor.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "ReviewPlan.ts")]
pub struct ReviewPlan {
    pub base: String,
    pub target: String,
    pub files: Vec<ReviewFile>,
}

/// Get the files changed between two commits, including which side exists.
fn get_review_files(
    repo_root: &std::path::Path,
    base: &str,
    target: &str,
    paths: &[String],
) -> Result<Vec<ReviewFile>> {
    let mut cmd = Command::new("git");
    cmd.args(["diff", "--name-status", "-z", "--no-renames", base, target]);
    if !paths.is_empty() {
        cmd.arg("--");
        cmd.args(paths);
    }
    let output = cmd
        .current_dir(repo_root)
        .output()
        .context("failed to get list of changed files")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(anyhow::anyhow!("failed to get changed files: {stderr}"));
    }

    let mut fields: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    if fields.last().is_some_and(|field| field.is_empty()) {
        fields.pop();
    }
    if !fields.len().is_multiple_of(2) {
        return Err(anyhow::anyhow!("git diff returned a status without a path"));
    }

    let mut files = Vec::with_capacity(fields.len() / 2);
    for fields in fields.chunks_exact(2) {
        let status =
            std::str::from_utf8(fields[0]).context("changed file status is not valid UTF-8")?;
        let path = std::str::from_utf8(fields[1])
            .context("changed file path is not valid UTF-8")?
            .to_string();
        let (base_exists, target_exists) = match status {
            "A" => (false, true),
            "D" => (true, false),
            "M" | "T" => (true, true),
            other => {
                return Err(anyhow::anyhow!(
                    "git diff returned unknown file status '{other}' for '{path}'"
                ))
            }
        };
        files.push(ReviewFile {
            path,
            base_exists,
            target_exists,
        });
    }

    Ok(files)
}

/// Compute the exact review inputs shared by editor integrations.
pub(crate) fn build_review_plan(
    repo_root: &std::path::Path,
    pod_name: &str,
    paths: &[String],
) -> Result<ReviewPlan> {
    let target_ref = format!("refs/rumpelpod/{pod_name}");
    let target_commit = format!("{target_ref}^{{commit}}");
    let ref_check = Command::new("git")
        .args(["rev-parse", "--verify", &target_commit])
        .current_dir(repo_root)
        .output()
        .context("failed to check pod ref")?;

    if !ref_check.status.success() {
        return Err(anyhow::anyhow!(
            "pod ref '{target_ref}' not found in host repository (pod has no commits yet)"
        ));
    }
    let target = String::from_utf8_lossy(&ref_check.stdout)
        .trim()
        .to_string();

    let head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .context("failed to get HEAD commit")?;

    if !head_output.status.success() {
        let stderr = String::from_utf8_lossy(&head_output.stderr);
        let stderr = stderr.trim();
        return Err(anyhow::anyhow!("failed to get HEAD commit: {stderr}"));
    }

    let host_head = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();
    let merge_base_output = Command::new("git")
        .args(["merge-base", &target, &host_head])
        .current_dir(repo_root)
        .output()
        .context("failed to compute merge base")?;

    if !merge_base_output.status.success() {
        let stderr = String::from_utf8_lossy(&merge_base_output.stderr);
        let stderr = stderr.trim();
        return Err(anyhow::anyhow!(
            "failed to compute merge base between '{target}' and HEAD:\n{stderr}"
        ));
    }

    let base = String::from_utf8_lossy(&merge_base_output.stdout)
        .trim()
        .to_string();
    let files = get_review_files(repo_root, &base, &target, paths)?;

    Ok(ReviewPlan {
        base,
        target,
        files,
    })
}
