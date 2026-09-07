// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Git sync setup between host repo and pods.
//!
//! Pods access the host repository directly via the HTTP server.  The
//! server routes requests to the host repo's `.git` directory (or
//! submodule git dirs).
//!
//! This module used to manage an intermediate "gateway" bare repository
//! that sat between host and pods.  That indirection is gone, but the
//! module name stuck.  What remains is a mix of host-repo hook
//! installation, submodule detection, and git-dir resolution.  These
//! could move to `git.rs` (submodule / git-dir helpers) and `daemon.rs`
//! (host setup), at which point this module can be deleted.
//!
//! # Ref namespace
//!
//! Host branches live at their native `refs/heads/*`.  The current HEAD
//! commit is tracked by the HTTP server as `refs/rumpelpod/host-head`.
//!
//! Pod branches live under `refs/rumpelpod/`:
//! - `refs/rumpelpod/<branch>@<pod>` for each branch in a pod.
//! - `refs/rumpelpod/<pod>` as a convenience shortcut for the primary branch
//!   (where branch name == pod name).
//!
//! ## Access control
//!
//! A pre-receive hook on the host repo restricts pod writes:
//! - A pod identified by POD_NAME can only push to `refs/rumpelpod/*@<pod>`
//!   and `refs/rumpelpod/<pod>` (primary shortcut).
//! - Reading (fetch) is unrestricted.
//!
//! The pod name is determined server-side by the git HTTP server based on
//! the bearer token.  The server sets `POD_NAME` which hooks can trust.
//!
//! `git log rumpelpod/<pod>` and friends resolve to `refs/rumpelpod/<pod>`
//! via `gitrevisions(7)` shorthand rule 2 (`refs/<refname>`).

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use log::error;

use crate::async_command::AsyncCommandExt;
use crate::command_ext::CommandExt;

/// Info about a submodule discovered in the host repo.
pub struct SubmoduleInfo {
    /// Path relative to the top-level repo (includes parent prefixes for nested submodules).
    pub displaypath: String,
}

/// Ensure all submodules (including nested ones) are checked out on the host.
///
/// `git submodule foreach --recursive` can only enumerate submodules that are
/// already initialized and checked out. This function ensures that is the case.
///
/// WARNING: this resets submodule working trees to the commits recorded by
/// their parents, so only call it during initial setup.
pub async fn init_submodules_recursive(repo_path: &Path) -> Result<()> {
    if !repo_path.join(".gitmodules").exists() {
        return Ok(());
    }
    Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ])
        .current_dir(repo_path)
        .success_async()
        .await
        .context("git submodule update --init --recursive failed")?;
    Ok(())
}

/// Detect submodules in the host repo by running `git submodule foreach --recursive`.
/// Returns submodules sorted by depth (parents before children) so that
/// level-by-level init works correctly for nested submodules.
/// Returns an empty vec when `.gitmodules` is absent or on any error.
///
/// Only enumerates submodules that are already initialized and checked out.
/// Call `init_submodules_recursive` first if nested submodules might not
/// be checked out yet.
pub fn detect_submodules(repo_path: &Path) -> Vec<SubmoduleInfo> {
    if !repo_path.join(".gitmodules").exists() {
        return Vec::new();
    }

    // Use tab as delimiter so paths/names containing spaces are handled.
    // $displaypath is the path relative to the top-level repo.
    let output = match Command::new("git")
        .args([
            "submodule",
            "foreach",
            "--recursive",
            "--quiet",
            "printf '%s\\t%s\\t%s\\n' \"$name\" \"$sm_path\" \"$displaypath\"",
        ])
        .current_dir(repo_path)
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let exit_code = o.status.code().unwrap_or(-1);
            let stderr = stderr.trim();
            error!("git submodule foreach failed (exit {exit_code}): {stderr}");
            return Vec::new();
        }
        Err(e) => {
            error!("failed to run git submodule foreach: {e}");
            return Vec::new();
        }
    };
    let mut subs: Vec<SubmoduleInfo> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let _name = parts.next()?;
            let _path = parts.next()?;
            let displaypath = parts.next()?.to_string();
            Some(SubmoduleInfo { displaypath })
        })
        .collect();

    // Sort by depth so parents come before children.
    subs.sort_by_key(|s| s.displaypath.matches('/').count());
    subs
}

/// Resolve the actual git directory for a worktree, handling gitlink files
/// used by absorbed submodules (where `.git` is a file containing
/// `gitdir: <path>` rather than a directory).
pub fn resolve_git_dir(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(path)
        .success()
        .context("git rev-parse --git-dir failed")?;
    let git_dir = String::from_utf8_lossy(&output).trim().to_string();
    let git_dir_path = PathBuf::from(&git_dir);
    if git_dir_path.is_absolute() {
        Ok(git_dir_path)
    } else {
        Ok(path.join(git_dir_path))
    }
}

/// Resolve git directories for all submodules, returning (displaypath, git_dir) pairs.
pub fn resolve_submodule_git_dirs(repo_path: &Path) -> Vec<(String, PathBuf)> {
    detect_submodules(repo_path)
        .into_iter()
        .filter_map(|sub| {
            let sub_workdir = repo_path.join(&sub.displaypath);
            let git_dir = resolve_git_dir(&sub_workdir).ok()?;
            // Canonicalize to get a stable absolute path.
            let git_dir = fs::canonicalize(&git_dir).unwrap_or(git_dir);
            Some((sub.displaypath, git_dir))
        })
        .collect()
}

/// Check if the given path is inside a git repository.
fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Set up the host repo for pod access.
///
/// Initializes submodules on first call.  Idempotent.  If the
/// `repo_path` is not a git repository, this function does nothing.
pub async fn setup_gateway(repo_path: &Path) -> Result<()> {
    if !is_git_repo(repo_path) {
        return Ok(());
    }

    // Only init submodules on first call -- it resets working trees.
    // Use a marker file to detect subsequent calls.
    let marker = repo_path.join(".git/rumpelpod-submodules-initialized");
    if !marker.exists() {
        init_submodules_recursive(repo_path).await?;
        let _ = fs::write(&marker, "");
    }

    Ok(())
}

/// Install host-repo hooks for access control and UI invalidation.
///
/// Must be called AFTER the devcontainer image is built so the hook is
/// not baked into the image via COPY (it references a host-side binary
/// that does not exist inside the container).
pub fn install_host_hooks(repo_path: &Path) -> Result<()> {
    if !is_git_repo(repo_path) {
        return Ok(());
    }

    let rumpel_exe = std::env::current_exe()
        .context("resolving rumpel binary path")?
        .to_string_lossy()
        .to_string();
    let rumpel_exe = crate::devcontainer::shell_escape(&rumpel_exe);
    let escaped_repo_path = crate::devcontainer::shell_escape(&repo_path.to_string_lossy());

    let hooks_dir = repo_path.join(".git").join("hooks");

    let pre_receive_block =
        format!("{HOST_PRE_RECEIVE_COMMENT}\nexec {rumpel_exe} git-hook host-pre-receive \"$@\"\n");
    install_host_hook(
        &hooks_dir,
        "pre-receive",
        &pre_receive_block,
        HostHookPlacement::AfterUser,
    )?;

    let post_receive_block = format!(
        "{HOST_POST_RECEIVE_COMMENT}\n\
         {rumpel_exe} git-hook host-post-receive --repo-path {escaped_repo_path} || \
         printf '%s\\n' 'warning: failed to notify the rumpelpod daemon' >&2\n"
    );
    install_host_hook(
        &hooks_dir,
        "post-receive",
        &post_receive_block,
        HostHookPlacement::BeforeUser,
    )?;

    Ok(())
}

// -- Hook installation helpers -----------------------------------------------

/// Comment prefix shared by all host hook comment lines.  Used by
/// `strip_host_hooks` and `remove_host_hooks` (in prepared_image.rs)
/// to clean hooks from images.
const HOST_HOOK_COMMENT_PREFIX: &str = "# Installed by rumpelpod (host";

/// Comment identifying the host pre-receive hook block.
const HOST_PRE_RECEIVE_COMMENT: &str = "# Installed by rumpelpod (host pre-receive)";
const HOST_POST_RECEIVE_COMMENT: &str = "# Installed by rumpelpod (host post-receive)";

#[derive(Clone, Copy)]
enum HostHookPlacement {
    BeforeUser,
    AfterUser,
}

/// Remove host hook blocks from a hook file's content, preserving any
/// other code (e.g. user hooks).  Each host block is identified by its
/// comment line (prefixed with HOST_HOOK_COMMENT_PREFIX); the comment
/// and the following invocation line are both removed.
pub fn strip_host_hooks(content: &str) -> String {
    let mut result = Vec::new();
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim().starts_with(HOST_HOOK_COMMENT_PREFIX) {
            // Skip the invocation line that follows the comment.
            lines.next();
        } else {
            result.push(line);
        }
    }
    let mut out = result.join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Install a managed block without preventing an existing user hook from running.
fn install_host_hook(
    hooks_dir: &Path,
    hook_name: &str,
    block: &str,
    placement: HostHookPlacement,
) -> Result<()> {
    fs::create_dir_all(hooks_dir).with_context(|| {
        let hooks_dir = hooks_dir.display();
        format!("Failed to create hooks directory: {hooks_dir}")
    })?;

    let hook_path = hooks_dir.join(hook_name);

    let metadata = match fs::symlink_metadata(&hook_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            let hook_path = hook_path.display();
            return Err(error).with_context(|| format!("Failed to inspect hook: {hook_path}"));
        }
    };
    if let Some(metadata) = &metadata {
        let hook_path = hook_path.display();
        if metadata.file_type().is_symlink() {
            return Err(anyhow::Error::msg(format!(
                "Refusing to install managed hook over symbolic link: {hook_path}"
            )));
        }
        if metadata.nlink() > 1 {
            return Err(anyhow::Error::msg(format!(
                "Refusing to install managed hook over multiply linked file: {hook_path}"
            )));
        }
    }
    let existing = match &metadata {
        Some(_) => Some(fs::read_to_string(&hook_path).with_context(|| {
            let hook_path = hook_path.display();
            format!("Failed to read hook: {hook_path}")
        })?),
        None => None,
    };
    let content = if let Some(existing) = &existing {
        let cleaned = strip_host_hooks(existing);
        combine_host_hook(&cleaned, block, placement)
    } else {
        format!("#!/bin/sh\n{block}")
    };
    if existing.as_deref() != Some(&content) {
        write_hook_atomically(&hook_path, &content)?;
    }

    let mut perms = fs::metadata(&hook_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook_path, perms)?;

    Ok(())
}

fn write_hook_atomically(hook_path: &Path, content: &str) -> Result<()> {
    let hooks_dir = hook_path.parent().context("hook path has no parent")?;
    let mut staged = tempfile::NamedTempFile::new_in(hooks_dir).with_context(|| {
        let hook_path = hook_path.display();
        format!("Failed to stage hook update: {hook_path}")
    })?;
    staged.write_all(content.as_bytes()).with_context(|| {
        let hook_path = hook_path.display();
        format!("Failed to write staged hook: {hook_path}")
    })?;
    let mut permissions = staged.as_file().metadata()?.permissions();
    permissions.set_mode(0o755);
    staged.as_file().set_permissions(permissions)?;
    staged.as_file().sync_all().with_context(|| {
        let hook_path = hook_path.display();
        format!("Failed to sync staged hook: {hook_path}")
    })?;
    staged
        .persist(hook_path)
        .map_err(|error| error.error)
        .with_context(|| {
            let hook_path = hook_path.display();
            format!("Failed to install hook: {hook_path}")
        })?;
    Ok(())
}

fn combine_host_hook(existing: &str, block: &str, placement: HostHookPlacement) -> String {
    match placement {
        HostHookPlacement::BeforeUser => {
            let block = block.trim_end();
            if let Some((shebang, body)) = existing.split_once('\n') {
                if shebang.starts_with("#!") {
                    return format!("{shebang}\n{block}\n{body}");
                }
            }
            if existing.starts_with("#!") {
                return format!("{existing}\n{block}\n");
            }
            format!("#!/bin/sh\n{block}\n{existing}")
        }
        HostHookPlacement::AfterUser => {
            let separator = if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{existing}{separator}{block}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn installing_post_receive_preserves_existing_hook() {
        let temporary = tempfile::tempdir().expect("create hook directory");
        let hooks = temporary.path();
        let hook = hooks.join("post-receive");
        let user_hook = "#!/bin/sh\nprintf 'user hook\\n'\n";
        fs::write(&hook, user_hook).expect("write user hook");
        let block =
            "# Installed by rumpelpod (host post-receive)\nrumpel git-hook host-post-receive\n";

        install_host_hook(hooks, "post-receive", block, HostHookPlacement::BeforeUser)
            .expect("install rumpelpod post-receive hook");
        let installed_inode = fs::metadata(&hook).expect("read installed metadata").ino();
        install_host_hook(hooks, "post-receive", block, HostHookPlacement::BeforeUser)
            .expect("reinstall rumpelpod post-receive hook");

        let installed = fs::read_to_string(&hook).expect("read installed hook");
        assert_eq!(
            fs::metadata(&hook)
                .expect("read reinstalled metadata")
                .ino(),
            installed_inode,
            "an unchanged hook was rewritten"
        );
        assert!(installed.starts_with("#!/bin/sh\n"));
        let notification_position = installed
            .find(HOST_POST_RECEIVE_COMMENT)
            .expect("installed notification");
        let user_hook_position = installed
            .find("printf 'user hook\\n'")
            .expect("preserved user hook");
        assert!(notification_position < user_hook_position);
        assert_eq!(installed.matches(HOST_POST_RECEIVE_COMMENT).count(), 1);
        let stripped = strip_host_hooks(&installed);
        assert!(stripped.contains("printf 'user hook\\n'"));
        assert!(!stripped.contains(HOST_POST_RECEIVE_COMMENT));
    }

    #[test]
    fn post_receive_notification_runs_before_user_exit() {
        let temporary = tempfile::tempdir().expect("create hook directory");
        let hooks = temporary.path();
        let hook = hooks.join("post-receive");
        fs::write(&hook, "#!/bin/sh\nexit 0\n").expect("write user hook");
        let notification = temporary.path().join("notification-ran");
        let notification = crate::devcontainer::shell_escape(&notification.to_string_lossy());
        let block = format!("{HOST_POST_RECEIVE_COMMENT}\nprintf notification > {notification}\n");

        install_host_hook(hooks, "post-receive", &block, HostHookPlacement::BeforeUser)
            .expect("install rumpelpod post-receive hook");
        Command::new(&hook)
            .success()
            .expect("run installed post-receive hook");

        assert!(temporary.path().join("notification-ran").exists());
    }

    #[test]
    fn post_receive_notification_failure_does_not_prevent_user_hook() {
        let temporary = tempfile::tempdir().expect("create hook directory");
        let hooks = temporary.path();
        let hook = hooks.join("post-receive");
        let user_notification = temporary.path().join("user-hook-ran");
        let user_notification =
            crate::devcontainer::shell_escape(&user_notification.to_string_lossy());
        fs::write(
            &hook,
            format!("#!/bin/sh -e\nprintf user > {user_notification}\n"),
        )
        .expect("write user hook");
        let block = format!(
            "{HOST_POST_RECEIVE_COMMENT}\n\
             false || printf '%s\\n' 'notification failed' >&2\n"
        );

        install_host_hook(hooks, "post-receive", &block, HostHookPlacement::BeforeUser)
            .expect("install rumpelpod post-receive hook");
        Command::new(&hook)
            .success()
            .expect("run installed post-receive hook");

        assert!(temporary.path().join("user-hook-ran").exists());
    }

    #[test]
    fn installing_host_hook_does_not_replace_non_utf8_user_hook() {
        let temporary = tempfile::tempdir().expect("create hook directory");
        let hooks = temporary.path();
        let hook = hooks.join("post-receive");
        let original = b"#!/bin/sh\n# \xff\n";
        fs::write(&hook, original).expect("write non-UTF-8 user hook");
        let block =
            "# Installed by rumpelpod (host post-receive)\nrumpel git-hook host-post-receive\n";

        let error = install_host_hook(hooks, "post-receive", block, HostHookPlacement::BeforeUser)
            .expect_err("non-UTF-8 hook must be preserved");

        assert!(format!("{error:#}").contains("Failed to read hook"));
        assert_eq!(fs::read(&hook).expect("read preserved hook"), original);
    }

    #[test]
    fn installing_host_hook_rejects_user_symlink() {
        let temporary = tempfile::tempdir().expect("create hook directory");
        let hooks = temporary.path();
        let target = hooks.join("shared-post-receive");
        let hook = hooks.join("post-receive");
        let original = "#!/bin/sh\nprintf 'user hook\\n'\n";
        fs::write(&target, original).expect("write hook target");
        symlink("shared-post-receive", &hook).expect("link user hook");
        let block =
            "# Installed by rumpelpod (host post-receive)\nrumpel git-hook host-post-receive\n";

        let error = install_host_hook(hooks, "post-receive", block, HostHookPlacement::BeforeUser)
            .expect_err("symlinked hook must be rejected");

        assert!(format!("{error:#}").contains("symbolic link"));
        assert!(
            fs::symlink_metadata(&hook)
                .expect("read installed hook metadata")
                .file_type()
                .is_symlink(),
            "installation replaced the user hook symlink"
        );
        let installed = fs::read_to_string(&target).expect("read installed hook target");
        assert_eq!(installed, original);
    }

    #[test]
    fn installing_host_hook_rejects_multiply_linked_file() {
        let temporary = tempfile::tempdir().expect("create hook directory");
        let hooks = temporary.path();
        let target = hooks.join("shared-post-receive");
        let hook = hooks.join("post-receive");
        let original = "#!/bin/sh\nprintf 'user hook\\n'\n";
        fs::write(&target, original).expect("write hook target");
        fs::hard_link(&target, &hook).expect("link user hook");
        let block =
            "# Installed by rumpelpod (host post-receive)\nrumpel git-hook host-post-receive\n";

        let error = install_host_hook(hooks, "post-receive", block, HostHookPlacement::BeforeUser)
            .expect_err("multiply linked hook must be rejected");

        assert!(format!("{error:#}").contains("multiply linked file"));
        assert_eq!(
            fs::read_to_string(&hook).expect("read preserved hook"),
            original
        );
        assert_eq!(
            fs::read_to_string(&target).expect("read preserved hook target"),
            original
        );
    }
}
