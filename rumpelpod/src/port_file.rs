// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Small helpers for managing the port files that the in-container
//! processes (container-serve, tunnel-server) use to advertise the
//! TCP port they picked.
//!
//! Rules:
//!
//! * If the file exists and is parseable, the process tries to rebind
//!   that port (and fails hard on collision).
//! * If the file is absent, the process binds `:0` and writes the
//!   chosen port atomically via tempfile + fsync + rename + parent
//!   fsync.
//! * Readers (in-container consumers like the Claude hook and
//!   `rumpel container-exec`) fail hard on missing or empty files.
//!   There are no default ports.
//! * The codex app-server port file is advertisement-only: the pod
//!   server always picks a fresh ephemeral port (a recorded one may
//!   belong to another pod under host networking) and routes all
//!   traffic via its in-memory copy of the port.

use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

pub const SERVER_PORT_FILE: &str = "/opt/rumpelpod/server-port";
pub const TUNNEL_PORT_FILE: &str = "/opt/rumpelpod/tunnel-port";
pub const CODEX_APP_SERVER_PORT_FILE: &str = "/opt/rumpelpod/codex-app-server-port";

/// Read and parse a port file.  Fails on missing, empty, or
/// unparseable content -- the caller has no fallback.
pub fn read_required(path: &Path) -> Result<u16> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading port file {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("port file {} is empty", path.display()));
    }
    trimmed
        .parse::<u16>()
        .with_context(|| format!("parsing port file {}", path.display()))
}

/// Read a port file if it exists.  Returns `Ok(None)` when the file is
/// absent; returns `Err` when the file exists but is unreadable, empty,
/// or unparseable (we never silently fall back to a default).
pub fn read_preferred(path: &Path) -> Result<Option<u16>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(anyhow::anyhow!(
                    "port file {} exists but is empty",
                    path.display()
                ));
            }
            let port = trimmed
                .parse::<u16>()
                .with_context(|| format!("parsing port file {}", path.display()))?;
            Ok(Some(port))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("reading port file {}", path.display())))
        }
    }
}

/// Atomically write `port` into `path`.  Uses tempfile + fsync +
/// rename + parent fsync so a concurrent reader can never observe a
/// partial or zero-length file.
pub fn write_atomic(path: &Path, port: u16) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("port file {} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".port.")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    tmp.write_all(port.to_string().as_bytes())
        .context("writing port value to temp file")?;
    // tempfile defaults to 0o600 (security-oriented), but the port
    // files are consumed by unrelated processes -- for instance a
    // tunnel-server running as root writes the tunnel-port file that
    // container-serve reads after dropping to the container user.
    // 0o644 keeps it readable across those boundaries.
    tmp.as_file()
        .set_permissions(Permissions::from_mode(0o644))
        .context("setting temp file permissions")?;
    tmp.as_file_mut().sync_all().context("fsync temp file")?;

    tmp.persist(path)
        .with_context(|| format!("renaming temp file into {}", path.display()))?;

    let dir = File::open(parent)
        .with_context(|| format!("opening parent dir {} for fsync", parent.display()))?;
    // fsync the parent directory so the rename is durable across crash.
    // SAFETY: no-op on tmpfs, essential on real filesystems.
    let ret = unsafe { libc::fsync(dir.as_raw_fd()) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("fsync parent dir {}", parent.display()));
    }
    Ok(())
}

/// Remove the port file so readers cannot observe stale content from
/// a prior (dead) process.  Ignores "not found" errors.
pub fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(anyhow::Error::new(e)
                .context(format!("removing stale port file {}", path.display())))
        }
    }
}
