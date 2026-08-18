// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Switch the current process to the container user recorded at image
//! build time.
//!
//! `prepare-image` writes the resolved user name to a well-known path.
//! At runtime, `container-exec` and `container-serve` call `switch_user`
//! early in main to drop from root to the correct user, including all
//! supplementary groups (e.g. docker).
//!
//! These entry points only run inside Linux containers, so the actual
//! switching logic is gated to Linux.  On macOS the binary runs on the
//! host only; `switch_user` is unreachable but must compile.

#[cfg(not(target_os = "macos"))]
use std::ffi::CString;

use anyhow::{Context, Result};
use nix::unistd::User;

/// Path where `prepare-image` stores the resolved container user name.
pub const USER_FILE: &str = "/opt/rumpelpod/user";

/// Read the container user from `USER_FILE`, resolve it in the passwd
/// database, and switch the process uid, gid, and supplementary groups.
///
/// After this call the process runs as the target user with all groups
/// (primary + supplementary) set.  `$HOME`, `$USER`, and `$LOGNAME`
/// are updated to match.
///
/// When already running as the target user, verifies that
/// supplementary groups are set correctly.
#[cfg(not(target_os = "macos"))]
pub fn switch_user() -> Result<()> {
    let name = std::fs::read_to_string(USER_FILE)
        .with_context(|| format!("reading container user from {USER_FILE}"))?;
    let user = resolve_user(name.trim())?;
    let name = &user.name;

    let cname = CString::new(name.as_str()).context("user name contains NUL")?;

    if nix::unistd::getuid() == user.uid {
        // Already the target user.  The container runtime should
        // have set supplementary groups; verify they are correct.
        verify_groups(&cname, user.gid)?;
    } else {
        // Need to switch (e.g. root -> non-root).  Set supplementary
        // groups first, then drop gid, then uid.  Order matters:
        // setuid before setgid would lose the privilege for setgid.
        nix::unistd::initgroups(&cname, user.gid)
            .with_context(|| format!("initgroups for '{name}' (gid {})", user.gid))?;
        nix::unistd::setgid(user.gid).with_context(|| format!("setgid to {}", user.gid))?;
        nix::unistd::setuid(user.uid).with_context(|| format!("setuid to {}", user.uid))?;
    }

    set_user_env(&user);

    Ok(())
}

/// container-exec and container-serve only run inside Linux containers.
/// This stub lets the code compile on macOS where the host binary is
/// built but never invokes switch_user.
#[cfg(target_os = "macos")]
pub fn switch_user() -> Result<()> {
    anyhow::bail!("switch_user is not supported on macOS (container-only code path)");
}

/// Check that the process's current supplementary groups match what
/// /etc/group prescribes for `name`.  This catches container runtimes
/// that fail to populate supplementary groups.
#[cfg(not(target_os = "macos"))]
fn verify_groups(cname: &std::ffi::CString, primary_gid: nix::unistd::Gid) -> Result<()> {
    // Collect the expected set: primary gid + all groups the user
    // belongs to in /etc/group.  This mirrors what initgroups does.
    let expected: std::collections::HashSet<u32> = {
        let mut set = std::collections::HashSet::new();
        set.insert(primary_gid.as_raw());
        // getgrouplist returns all groups for the user.
        if let Ok(groups) = nix::unistd::getgrouplist(cname, primary_gid) {
            for g in groups {
                set.insert(g.as_raw());
            }
        }
        set
    };

    let actual: std::collections::HashSet<u32> = nix::unistd::getgroups()
        .context("getgroups")?
        .into_iter()
        .map(|g| g.as_raw())
        .collect();

    let missing: Vec<u32> = expected.difference(&actual).copied().collect();
    if !missing.is_empty() {
        let missing_str: Vec<String> = missing.iter().map(|g| g.to_string()).collect();
        return Err(anyhow::anyhow!(
            "process is missing supplementary groups: {}\n\
             Expected groups {:?}, got {:?}.\n\
             The container runtime did not set supplementary groups correctly.",
            missing_str.join(", "),
            expected,
            actual,
        ));
    }

    Ok(())
}

/// Resolve a user by name, falling back to UID lookup for numeric strings.
pub(crate) fn resolve_user(name: &str) -> Result<nix::unistd::User> {
    if let Some(user) =
        User::from_name(name).with_context(|| format!("looking up user '{name}'"))?
    {
        return Ok(user);
    }
    if let Ok(uid) = name.parse::<u32>() {
        if let Some(user) = User::from_uid(nix::unistd::Uid::from_raw(uid))
            .with_context(|| format!("looking up uid {uid}"))?
        {
            return Ok(user);
        }
    }
    Err(anyhow::anyhow!("user '{name}' not found"))
}

#[cfg(not(target_os = "macos"))]
fn set_user_env(user: &nix::unistd::User) {
    let home = user.dir.to_string_lossy();
    std::env::set_var("HOME", &*home);
    std::env::set_var("USER", &user.name);
    std::env::set_var("LOGNAME", &user.name);
}
