// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::cli::MergeCommand;
use crate::config::{load_json_config, DescriptionMode};
use crate::daemon;
use crate::daemon::protocol::{Daemon, DaemonClient, ListPodsRequest, PodName};
use crate::enter;
use crate::git::get_repo_root;

/// Check for uncommitted changes inside the pod container.
/// Prints a warning to stderr if the working tree is dirty.
fn check_dirty_checkout(pod_name: &str) -> Result<()> {
    let result = enter::launch_pod(pod_name, None)?;
    let container_repo_path = result.container_repo_path.clone();

    let pod = crate::pod::PodClient::connect(&result.container_url, &result.container_token)?;
    let run_result = pod
        .run(
            &[
                "git",
                "-C",
                &container_repo_path.to_string_lossy(),
                "status",
                "--porcelain",
            ],
            None,
            &[],
            None,
            Some(30),
        )
        .context("failed to check pod working tree status")?;

    if run_result.exit_code != 0 {
        use base64::Engine;
        let stderr = base64::engine::general_purpose::STANDARD
            .decode(&run_result.stderr)
            .unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr);
        let stderr = stderr.trim();
        return Err(anyhow::anyhow!(
            "git status in pod '{pod_name}' failed: {stderr}"
        ));
    }

    use base64::Engine;
    let stdout = base64::engine::general_purpose::STANDARD
        .decode(&run_result.stdout)
        .unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout);
    if !stdout.trim().is_empty() {
        eprintln!("warning: pod '{pod_name}' has uncommitted changes");
    }

    Ok(())
}

// -- description file helpers ------------------------------------------------

struct Description {
    content: String,
    path: String,
}

/// Read a file from a git ref. Returns None if the file does not exist.
fn read_file_from_ref(repo_root: &Path, git_ref: &str, path: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["show", &format!("{git_ref}:{path}")])
        .current_dir(repo_root)
        .stderr(Stdio::null())
        .output()
        .context("failed to read file from git ref")?;

    if !output.status.success() {
        return Ok(None);
    }

    let contents =
        String::from_utf8(output.stdout).context("description file is not valid UTF-8")?;
    let trimmed = contents.trim_end();

    if trimmed.is_empty() {
        return Ok(None);
    }

    Ok(Some(trimmed.to_string()))
}

/// Check whether a file exists in a git ref.
fn file_exists_in_ref(repo_root: &Path, git_ref: &str, path: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["cat-file", "-e", &format!("{git_ref}:{path}")])
        .current_dir(repo_root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to check file in git ref")?;
    Ok(status.success())
}

/// Decide whether to use a description file for the merge commit message.
///
/// Resolution order: CLI flags > config.
///
/// --description-file <path> requires the file to exist on the pod branch.
/// Config mode "auto" uses it if found, "required" fails if absent.
fn resolve_description(
    repo_root: &Path,
    pod_ref: &str,
    mode: DescriptionMode,
    config_path: Option<&str>,
    cli_file: Option<&str>,
    cli_no_file: bool,
) -> Result<Option<Description>> {
    if cli_no_file {
        return Ok(None);
    }

    // --description-file requires the file to exist
    if let Some(path) = cli_file {
        return match read_file_from_ref(repo_root, pod_ref, path)? {
            Some(content) => Ok(Some(Description {
                content,
                path: path.to_string(),
            })),
            None => Err(anyhow::anyhow!(
                "description file '{path}' not found on pod branch"
            )),
        };
    }

    let path = match config_path {
        Some(p) => p,
        // Mode is off, or somehow no path -- nothing to do.
        None => return Ok(None),
    };

    match mode {
        DescriptionMode::Off => Ok(None),
        DescriptionMode::Auto => Ok(
            read_file_from_ref(repo_root, pod_ref, path)?.map(|content| Description {
                content,
                path: path.to_string(),
            }),
        ),
        DescriptionMode::Required => match read_file_from_ref(repo_root, pod_ref, path)? {
            Some(content) => Ok(Some(Description {
                content,
                path: path.to_string(),
            })),
            None => Err(anyhow::anyhow!(
                "description file '{path}' not found on pod branch (required by config)"
            )),
        },
    }
}

/// Remove the description file from HEAD and amend the commit.
fn remove_description_file(repo_root: &Path, description_path: &str) -> Result<()> {
    if !file_exists_in_ref(repo_root, "HEAD", description_path)? {
        return Ok(());
    }

    let rm_status = Command::new("git")
        .args(["rm", description_path])
        .current_dir(repo_root)
        .stdout(Stdio::null())
        .status()
        .context("failed to git rm description file")?;

    if !rm_status.success() {
        return Err(anyhow::anyhow!("git rm {description_path} failed"));
    }

    let amend_status = Command::new("git")
        .args(["commit", "--amend", "--no-edit"])
        .current_dir(repo_root)
        .status()
        .context("failed to amend merge commit")?;

    if !amend_status.success() {
        return Err(anyhow::anyhow!(
            "failed to amend merge commit to remove {description_path}"
        ));
    }

    Ok(())
}

// -- main entry point --------------------------------------------------------

pub fn merge(cmd: &MergeCommand) -> Result<()> {
    let repo_root = get_repo_root()?;
    let json_config = load_json_config(&repo_root)?;
    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);

    // 1. Verify the pod exists in the daemon
    let pods = client.list_pods(ListPodsRequest::synchronized(repo_root.clone()))?;
    if !pods.iter().any(|p| p.name == cmd.name) {
        let name = &cmd.name;
        return Err(anyhow::anyhow!("pod '{name}' not found"));
    }

    // 2. Verify the pod ref exists
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

    // 3. Warn about uncommitted changes in the pod
    check_dirty_checkout(&cmd.name)?;

    // 4. Check if there is nothing to merge (pod ref is ancestor of HEAD)
    let ancestor_check = Command::new("git")
        .args(["merge-base", "--is-ancestor", &pod_ref, "HEAD"])
        .current_dir(&repo_root)
        .status()
        .context("failed to run merge-base --is-ancestor")?;

    if ancestor_check.success() {
        let name = &cmd.name;
        eprintln!("warning: nothing to merge, host is up to date with pod '{name}'");
        let pod_name = PodName::new(cmd.name.clone()).map_err(|e| anyhow::anyhow!(e))?;
        client.stop_pod(pod_name, repo_root, false)?;
        return Ok(());
    }

    // 5. Resolve the description file
    let description = resolve_description(
        &repo_root,
        &pod_ref,
        json_config.merge.description,
        json_config.merge.description_file_path(),
        cmd.description_file.as_deref(),
        cmd.no_description_file,
    )?;

    // 6. Check that the merge is clean before touching the working tree
    let merge_tree = Command::new("git")
        .args(["merge-tree", "HEAD", &pod_ref])
        .current_dir(&repo_root)
        .output()
        .context("failed to run git merge-tree")?;

    if !merge_tree.status.success() {
        return Err(anyhow::anyhow!(
            "merge would produce conflicts, aborting without changing the working tree"
        ));
    }

    // 7. Run git merge (known to be clean)
    let mut merge_cmd = Command::new("git");
    merge_cmd.arg("merge");

    match description {
        Some(ref desc) => {
            if !cmd.git_args.iter().any(|a| a == "--no-ff") {
                merge_cmd.arg("--no-ff");
            }
            merge_cmd.args(["-m", &desc.content]);
        }
        None => {
            // No description file -- open an editor so the user can write
            // the merge commit message.  Placed before git_args so the
            // user can override with --no-edit if desired.
            merge_cmd.arg("--edit");
        }
    }

    for arg in &cmd.git_args {
        merge_cmd.arg(arg);
    }
    merge_cmd.arg(&pod_ref);
    merge_cmd.current_dir(&repo_root);

    let merge_status = merge_cmd.status().context("failed to run git merge")?;

    if !merge_status.success() {
        let code = merge_status.code().unwrap_or(-1);
        return Err(anyhow::anyhow!("git merge exited with status {code}"));
    }

    // 7. Remove the description file from the merge result
    if let Some(ref desc) = description {
        remove_description_file(&repo_root, &desc.path)?;
    }

    // Only stop pod after successful merge
    let pod_name = PodName::new(cmd.name.clone()).map_err(|e| anyhow::anyhow!(e))?;
    client.stop_pod(pod_name, repo_root, false)?;

    Ok(())
}
