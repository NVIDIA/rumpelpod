// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::CpCommand;
use crate::daemon;
use crate::daemon::protocol::{Daemon, DaemonClient};
use crate::enter;
use crate::git::get_repo_root;
use crate::pod::PodClient;

#[derive(Debug)]
enum CopyDirection {
    FromPod {
        pod_name: String,
        container_path: String,
        local_path: String,
    },
    ToPod {
        pod_name: String,
        local_path: String,
        container_path: String,
    },
}

/// Split a POD:PATH argument on the first colon.
/// Returns None for `-` (stdin/stdout) or strings starting with `:`,
/// since those are not pod references.
fn parse_pod_path(arg: &str) -> Option<(&str, &str)> {
    if arg == "-" {
        return None;
    }
    let colon_pos = arg.find(':')?;
    if colon_pos == 0 {
        return None;
    }
    Some((&arg[..colon_pos], &arg[colon_pos + 1..]))
}

fn parse_direction(src: &str, dest: &str) -> Result<CopyDirection> {
    let src_pod = parse_pod_path(src);
    let dest_pod = parse_pod_path(dest);

    match (src_pod, dest_pod) {
        (Some((pod, path)), None) => Ok(CopyDirection::FromPod {
            pod_name: pod.to_string(),
            container_path: path.to_string(),
            local_path: dest.to_string(),
        }),
        (None, Some((pod, path))) => Ok(CopyDirection::ToPod {
            pod_name: pod.to_string(),
            local_path: src.to_string(),
            container_path: path.to_string(),
        }),
        (None, None) => Err(anyhow::anyhow!(
            "neither src nor dest uses POD:PATH syntax.\n\
                 Exactly one of src or dest must specify a pod, e.g. my-pod:/path/in/container"
        )),
        (Some(_), Some(_)) => Err(anyhow::anyhow!(
            "both src and dest use POD:PATH syntax.\n\
                 Exactly one side must be a local path."
        )),
    }
}

/// Resolve a container path: if relative, make it relative to the container repo root.
fn resolve_container_path(container_path: &str, container_repo_path: &Path) -> String {
    if Path::new(container_path).is_absolute() {
        container_path.to_string()
    } else {
        container_repo_path
            .join(container_path)
            .to_string_lossy()
            .into_owned()
    }
}

/// Stream a tar archive of a local path through a pipe.
///
/// The archive always has a single top-level entry under the wrapper
/// directory `_/`: either `_/<filename>` for a file, or `_/<dirname>/...`
/// for a directory. This lets the receiver distinguish files from
/// directories by inspecting the tar structure without out-of-band metadata.
///
/// Returns a readable end of the pipe and a handle to the writer thread.
/// The caller must read from the pipe before joining the handle to avoid
/// deadlock (pipes have a finite buffer).
fn tar_local_path(
    local_path: &str,
    follow_symlinks: bool,
) -> Result<(std::io::PipeReader, std::thread::JoinHandle<Result<()>>)> {
    let path = PathBuf::from(local_path);
    let path_display = path.display().to_string();
    let meta = std::fs::symlink_metadata(&path).with_context(|| format!("stat {path_display}"))?;

    let (read_end, write_end) = std::io::pipe().context("creating pipe")?;

    let handle = std::thread::spawn(move || {
        let path_display = path.display();
        let mut archive = tar::Builder::new(write_end);
        archive.follow_symlinks(follow_symlinks);

        let name = path
            .file_name()
            .with_context(|| format!("no file name in {path_display}"))?;
        let wrapper_name = Path::new("_").join(name);

        if meta.is_dir() {
            archive
                .append_dir_all(&wrapper_name, &path)
                .with_context(|| format!("archiving directory {path_display}"))?;
        } else {
            archive
                .append_path_with_name(&path, &wrapper_name)
                .with_context(|| format!("archiving file {path_display}"))?;
        }

        archive
            .into_inner()
            .with_context(|| format!("finalizing tar for {path_display}"))?;
        Ok(())
    });

    Ok((read_end, handle))
}

/// Extract a tar archive to a local path.
///
/// Expects the wrapper format produced by `tar_local_path` /
/// `cp_download_impl`: every entry lives under `_/<name>/...` or is
/// `_/<name>` for a single file. The first path component (`_`) and
/// the second (`<name>`) are stripped, and remaining paths are placed
/// under `local_path`.
fn untar_to_local(reader: impl Read, local_path: &str) -> Result<()> {
    let dest = Path::new(local_path);
    let dest_display = dest.display();
    // Match standard cp: when the destination is an existing directory,
    // place the content inside it rather than overwriting it.
    let dest_is_dir = dest.is_dir();

    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let path = entry.path().context("reading entry path")?.into_owned();

        // Strip the `_/` wrapper prefix; skip the wrapper dir entry itself.
        let relative = match path.strip_prefix("_") {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };

        let target = if dest_is_dir {
            dest.join(relative)
        } else {
            // Strip the content name (file name or directory name), leaving
            // the path relative to the destination.
            let mut components = relative.components();
            components.next();
            let rest: PathBuf = components.collect();

            if rest.as_os_str().is_empty() {
                dest.to_path_buf()
            } else {
                dest.join(&rest)
            }
        };

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).with_context(|| format!("creating {dest_display}"))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating parent for {dest_display}"))?;
            }
            let target_display = target.display();
            entry
                .unpack(&target)
                .with_context(|| format!("extracting to {target_display}"))?;
        }
    }

    Ok(())
}

pub fn cp(cmd: &CpCommand) -> Result<()> {
    let direction = parse_direction(&cmd.src, &cmd.dest)?;

    let pod_name = match &direction {
        CopyDirection::FromPod { pod_name, .. } => pod_name.as_str(),
        CopyDirection::ToPod { pod_name, .. } => pod_name.as_str(),
    };

    let repo_root = get_repo_root()?;
    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);
    let pods = client.list_pods(repo_root)?;
    if !pods.iter().any(|p| p.name == pod_name) {
        return Err(anyhow::anyhow!("pod '{pod_name}' does not exist"));
    }

    let result = enter::launch_pod(pod_name, cmd.host_args.resolve()?)?;
    let repo_path = result.container_repo_path.clone();
    let client = PodClient::connect(&result.container_url, &result.container_token)?;

    match &direction {
        CopyDirection::FromPod {
            container_path,
            local_path,
            ..
        } => {
            let resolved = resolve_container_path(container_path, &repo_path);
            let response = client.cp_download(Path::new(&resolved), cmd.follow_link)?;
            untar_to_local(response, local_path)?;
        }
        CopyDirection::ToPod {
            local_path,
            container_path,
            ..
        } => {
            let resolved = resolve_container_path(container_path, &repo_path);
            let (reader, handle) = tar_local_path(local_path, cmd.follow_link)?;
            client.cp_upload(Path::new(&resolved), reader)?;
            handle
                .join()
                .expect("tar writer panicked")
                .context("archiving local path")?;
        }
    }

    Ok(())
}
