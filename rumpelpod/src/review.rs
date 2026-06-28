// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Review pod changes using git difftool.
//!
//! This module implements the `rumpel review` command, which shows the diff
//! between a pod's primary branch and the merge base with the host's current HEAD.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use tempfile::TempDir;

use crate::cli::ReviewCommand;
use crate::daemon;
use crate::daemon::protocol::{Daemon, DaemonClient, ListPodsRequest};
use crate::git::get_repo_root;

/// Translate a difftool name to its executable path.
/// This mirrors Git's `translate_merge_tool_path()` logic from the mergetools/ directory.
///
/// Git has built-in knowledge of certain tool aliases that map to different executables.
/// For example, "nvimdiff" is not a real executable - it needs to be translated to "nvim".
///
/// This function implements the same mappings as Git's built-in mergetool definitions.
fn translate_tool_path(tool: &str) -> String {
    // vimdiff variants (from mergetools/vimdiff)
    // These cover: vimdiff, vimdiff1, vimdiff2, vimdiff3 and g/n prefixed versions
    if tool.starts_with("nvimdiff") {
        return "nvim".to_string();
    }
    if tool.starts_with("gvimdiff") {
        return "gvim".to_string();
    }
    if tool.starts_with("vimdiff") {
        return "vim".to_string();
    }

    // Other common tool mappings from Git's mergetools/
    match tool {
        "araxis" => "compare".to_string(),
        "emerge" => "emacs".to_string(),
        "vscode" => "code".to_string(),
        "deltawalker" => "DeltaWalker".to_string(),

        // bc (Beyond Compare) - we use the simpler form here
        // Git checks if bcomp exists first, but we'll let the OS handle that
        "bc" | "bc3" | "bc4" => "bcomp".to_string(),

        // Tools where the name matches the executable (no translation needed)
        // Including: meld, kdiff3, diffuse, kompare, tkdiff, xxdiff, opendiff,
        // p4merge, smerge, diffmerge, ecmerge, guiffy, tortoisemerge, etc.
        _ => tool.to_string(),
    }
}

/// Get the configured difftool name from git config.
/// Returns Ok(None) if no difftool is configured.
fn get_difftool_name(repo_root: &std::path::Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", "--get", "diff.tool"])
        .current_dir(repo_root)
        .output()
        .context("failed to query git config for diff.tool")?;

    if !output.status.success() {
        return Ok(None);
    }

    let tool = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if tool.is_empty() {
        return Ok(None);
    }

    Ok(Some(tool))
}

/// Get the executable path for a difftool.
/// This mirrors Git's logic in git-mergetool--lib.sh:
/// 1. Check difftool.<tool>.path config
/// 2. Check mergetool.<tool>.path config  
/// 3. Fall back to translate_tool_path()
fn get_tool_path(repo_root: &std::path::Path, tool: &str) -> Result<String> {
    // First check difftool.<tool>.path
    let difftool_path_key = format!("difftool.{tool}.path");
    let output = Command::new("git")
        .args(["config", "--get", &difftool_path_key])
        .current_dir(repo_root)
        .output()
        .context("failed to query git config for difftool path")?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Ok(path);
        }
    }

    // Then check mergetool.<tool>.path
    let mergetool_path_key = format!("mergetool.{tool}.path");
    let output = Command::new("git")
        .args(["config", "--get", &mergetool_path_key])
        .current_dir(repo_root)
        .output()
        .context("failed to query git config for mergetool path")?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Ok(path);
        }
    }

    // Fall back to translating the tool name
    Ok(translate_tool_path(tool))
}

/// Get the command for a difftool from git config.
/// Returns None if no custom command is configured (meaning it's a built-in tool).
fn get_difftool_cmd(repo_root: &std::path::Path, tool: &str) -> Result<Option<String>> {
    let config_key = format!("difftool.{tool}.cmd");
    let output = Command::new("git")
        .args(["config", "--get", &config_key])
        .current_dir(repo_root)
        .output()
        .context("failed to query git config for difftool cmd")?;

    if !output.status.success() {
        // No custom command configured - tool is either built-in or will be invoked directly
        return Ok(None);
    }

    let cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if cmd.is_empty() {
        Ok(None)
    } else {
        Ok(Some(cmd))
    }
}

/// Get the list of files changed between two commits,
/// optionally restricted to specific paths (like git diff -- <path>...).
fn get_changed_files(
    repo_root: &std::path::Path,
    base: &str,
    target: &str,
    paths: &[String],
) -> Result<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.args(["diff", "--name-only", base, target]);
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

    let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    Ok(files)
}

/// Get the content of a file at a specific commit.
/// Returns Ok(None) if the file doesn't exist at that commit.
fn get_file_at_commit(
    repo_root: &std::path::Path,
    commit: &str,
    file_path: &str,
) -> Result<Option<Vec<u8>>> {
    let output = Command::new("git")
        .args(["show", &format!("{commit}:{file_path}")])
        .current_dir(repo_root)
        .output()
        .context("failed to get file content from commit")?;

    if !output.status.success() {
        // File doesn't exist at this commit (new or deleted file)
        return Ok(None);
    }

    Ok(Some(output.stdout))
}

/// Write content to a temporary file.
fn write_temp_file(dir: &std::path::Path, name: &str, content: Option<&[u8]>) -> Result<PathBuf> {
    let path = dir.join(name);

    // Create parent directories if the file path contains subdirectories
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create parent directories")?;
    }

    let mut file = File::create(&path).context("failed to create temp file")?;
    if let Some(content) = content {
        file.write_all(content)
            .context("failed to write temp file")?;
    }

    Ok(path)
}

/// Check if a tool is a vimdiff variant (vimdiff, nvimdiff, gvimdiff).
fn is_vimdiff_variant(tool: &str) -> bool {
    tool.starts_with("vimdiff") || tool.starts_with("nvimdiff") || tool.starts_with("gvimdiff")
}

/// Invoke the difftool for a pair of files.
fn invoke_difftool(
    tool: &str,
    tool_path: &str,
    cmd_template: Option<&str>,
    local_path: &PathBuf,
    remote_path: &PathBuf,
) -> Result<()> {
    if let Some(template) = cmd_template {
        // Custom command with $LOCAL and $REMOTE placeholders
        let cmd = template
            .replace("$LOCAL", &local_path.to_string_lossy())
            .replace("$REMOTE", &remote_path.to_string_lossy());

        Command::new("sh")
            .args(["-c", &cmd])
            .status()
            .context("failed to run difftool command")?;
    } else if is_vimdiff_variant(tool) {
        // vimdiff/nvimdiff/gvimdiff need special handling to enable diff mode.
        // This mirrors git's diff_cmd() in mergetools/vimdiff:
        //   "$merge_tool_path" -R -f -d -c 'wincmd l' "$LOCAL" "$REMOTE"
        Command::new(tool_path)
            .args(["-R", "-f", "-d"])
            .arg("-c")
            .arg("wincmd l")
            .arg(local_path)
            .arg(remote_path)
            .status()
            .context("failed to run difftool")?;
    } else if tool == "vscode" {
        // Mirrors git's mergetools/vscode: without --wait --diff, VS Code
        // returns immediately and does not render the two files as a diff.
        Command::new(tool_path)
            .args(["--wait", "--diff"])
            .arg(local_path)
            .arg(remote_path)
            .status()
            .context("failed to run difftool")?;
    } else {
        // Other built-in tools - invoke the executable path directly with two file arguments
        Command::new(tool_path)
            .arg(local_path)
            .arg(remote_path)
            .status()
            .context("failed to run difftool")?;
    }

    // Note: We don't check the exit status here. Many difftools return
    // non-zero when files differ, and git difftool doesn't propagate
    // individual tool exit codes as errors.

    Ok(())
}

/// Prompt the user before opening a file in the difftool.
/// Returns true if the user wants to view the file, false to skip.
/// This matches git difftool's prompt format:
///   Viewing (1/16): '.rumpelpod.json'
///   Launch 'nvimdiff' [Y/n]?
fn prompt_for_file(
    file_path: &str,
    file_index: usize,
    total_files: usize,
    tool: &str,
) -> Result<bool> {
    let display_index = file_index + 1;
    println!("\nViewing ({display_index}/{total_files}): '{file_path}'");
    print!("Launch '{tool}' [Y/n]? ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read user input")?;

    let input = input.trim().to_lowercase();
    // Default is Yes (capital Y), so empty input or 'y'/'yes' means proceed
    Ok(input.is_empty() || input == "y" || input == "yes")
}

pub fn review(cmd: &ReviewCommand) -> Result<()> {
    let repo_root = get_repo_root()?;

    // Verify the pod exists in the daemon database before doing
    // anything else -- gives a clear error instead of a confusing
    // "ref not found" message for typos or deleted pods.
    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);
    let pods = client.list_pods(ListPodsRequest::synchronized(repo_root.clone()))?;
    if !pods.iter().any(|s| s.name == cmd.name) {
        let name = &cmd.name;
        return Err(anyhow::anyhow!("pod '{name}' does not exist"));
    }

    let name = &cmd.name;
    let pod_ref = format!("refs/rumpelpod/{name}");
    let ref_check = Command::new("git")
        .args(["rev-parse", "--verify", &pod_ref])
        .current_dir(&repo_root)
        .output()
        .context("failed to check pod ref")?;

    if !ref_check.status.success() {
        return Err(anyhow::anyhow!(
            "pod ref '{pod_ref}' not found in host repository (pod has no commits yet)"
        ));
    }

    // Get the current HEAD commit on the host
    let head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo_root)
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

    // Compute the merge base between the pod branch and the host HEAD
    let merge_base_output = Command::new("git")
        .args(["merge-base", &pod_ref, &host_head])
        .current_dir(&repo_root)
        .output()
        .context("failed to compute merge base")?;

    if !merge_base_output.status.success() {
        let stderr = String::from_utf8_lossy(&merge_base_output.stderr);
        let stderr = stderr.trim();
        return Err(anyhow::anyhow!(
            "failed to compute merge base between '{pod_ref}' and HEAD:\n{stderr}"
        ));
    }

    let merge_base = String::from_utf8_lossy(&merge_base_output.stdout)
        .trim()
        .to_string();

    // Get the configured difftool, or fall back to VS Code.
    let (tool, cmd_template, tool_path) = match get_difftool_name(&repo_root)? {
        Some(tool) => {
            let cmd_template = get_difftool_cmd(&repo_root, &tool)?;
            let tool_path = get_tool_path(&repo_root, &tool)?;
            (tool, cmd_template, tool_path)
        }
        None => {
            // No difftool configured, fall back to VS Code if available,
            // otherwise fail rather than silently doing nothing.
            let code_path = crate::which("code").ok_or_else(|| {
                anyhow::anyhow!(indoc::indoc! {"
                    no difftool configured and 'code' (VS Code CLI) not on PATH
                    install VS Code, or set a difftool in git config:
                        git config --global diff.tool vscode
                "})
            })?;
            eprintln!(
                "{}",
                indoc::indoc! {r#"
                    warning: no difftool configured, using VS Code
                    set one in ~/.gitconfig to silence this:

                        [diff]
                            tool = vscode
                "#}
            );
            (
                "vscode".to_string(),
                None,
                code_path.to_string_lossy().into_owned(),
            )
        }
    };

    // Get the list of changed files
    let changed_files = get_changed_files(&repo_root, &merge_base, &pod_ref, &cmd.paths)?;

    if changed_files.is_empty() {
        // No changes to review
        return Ok(());
    }

    // Create a temporary directory for the diff files (cleaned up on drop)
    let temp_dir = TempDir::with_prefix("rumpelpod-review-")?;
    let local_dir = temp_dir.path().join("local");
    let remote_dir = temp_dir.path().join("remote");
    fs::create_dir_all(&local_dir).context("failed to create local temp dir")?;
    fs::create_dir_all(&remote_dir).context("failed to create remote temp dir")?;

    // Process each changed file
    let total_files = changed_files.len();
    for (file_index, file_path) in changed_files.iter().enumerate() {
        // Prompt before opening each file (unless --yes flag is set)
        if !cmd.yes && !prompt_for_file(file_path, file_index, total_files, &tool)? {
            continue;
        }

        // Get file content at merge base (local/old version)
        let local_content = get_file_at_commit(&repo_root, &merge_base, file_path)?;

        // Get file content at pod ref (remote/new version)
        let remote_content = get_file_at_commit(&repo_root, &pod_ref, file_path)?;

        // Write temp files
        let local_file = write_temp_file(&local_dir, file_path, local_content.as_deref())?;
        let remote_file = write_temp_file(&remote_dir, file_path, remote_content.as_deref())?;

        // Invoke the difftool
        invoke_difftool(
            &tool,
            &tool_path,
            cmd_template.as_deref(),
            &local_file,
            &remote_file,
        )?;
    }

    Ok(())
}
