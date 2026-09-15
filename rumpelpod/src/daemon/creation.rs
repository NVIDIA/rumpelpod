// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{db, DaemonServer};
use crate::config::Host;

pub(crate) fn id(token: &str) -> String {
    // Labels are public backend metadata; never expose the pod's auth token.
    hex::encode(Sha256::digest(token.as_bytes()))[..32].to_string()
}

#[derive(Serialize, Deserialize)]
struct PendingCreate {
    repo_path: PathBuf,
    pod_name: String,
    host: Host,
    creation: String,
}

fn directory() -> Result<PathBuf> {
    Ok(db::db_path()?.with_file_name("pending-creations"))
}

// Persist before sending a mutating request. Cancellation or daemon death
// cannot tell us whether the backend will still act on that request.
pub(super) struct CreateIntent(PathBuf);

impl CreateIntent {
    pub(super) fn new(repo_path: &Path, pod_name: &str, host: &Host, token: &str) -> Result<Self> {
        let directory = directory()?;
        fs::create_dir_all(&directory).context("creating pending creation directory")?;
        let creation = id(token);
        let path = directory.join(&creation);
        let record = PendingCreate {
            repo_path: repo_path.to_path_buf(),
            pod_name: pod_name.to_string(),
            host: host.clone(),
            creation,
        };
        let mut file = tempfile::NamedTempFile::new_in(&directory)?;
        file.write_all(&serde_json::to_vec(&record)?)?;
        file.as_file().sync_all()?;
        file.persist(&path).context("persisting pending creation")?;
        fs::File::open(directory)?.sync_all()?;
        Ok(Self(path))
    }

    pub(super) fn confirmed(self) -> Result<()> {
        fs::remove_file(&self.0).context("confirming backend creation")
    }
}

impl DaemonServer {
    pub(super) fn start_creation_cleanup(self: &Arc<Self>) {
        let daemon = self.clone();
        crate::async_runtime::RUNTIME.spawn(async move {
            let interval = if super::is_test_mode() {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(30)
            };
            let mut ticks = tokio::time::interval(interval);
            loop {
                ticks.tick().await;
                if let Err(error) = daemon.cleanup_pending_creations().await {
                    log::error!("cleaning up unconfirmed creations: {error:#}");
                }
            }
        });
    }

    async fn cleanup_pending_creations(&self) -> Result<()> {
        let entries = match fs::read_dir(directory()?) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            // Atomic writes use a temporary filename until the full record
            // is durable. Only finalized identity filenames are actionable.
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() != 32 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let bytes = match fs::read(entry.path()) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let pending: PendingCreate = serde_json::from_slice(&bytes)?;
            let lock = self
                .lifecycle_locks
                .for_pod(&pending.repo_path, &pending.pod_name);
            let Ok(_lease) = lock.lease.clone().try_lock_owned() else {
                continue;
            };
            let wanted = {
                let conn = self.db.lock().unwrap();
                db::get_pod(&conn, &pending.repo_path, &pending.pod_name)?
                    .is_some_and(|pod| id(&pod.token) == pending.creation)
            };
            if wanted {
                continue;
            }
            let result = tokio::time::timeout(Duration::from_secs(15), async {
                self.host_executor_async(&pending.host)
                    .await?
                    .delete_creation_async(&pending.creation)
                    .await
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::error!("cleaning up creation {name}: {error:#}"),
                Err(error) => log::error!("cleaning up creation {name}: {error}"),
            }
            // Keep the intent even after NotFound: an accepted request may
            // still finish later. The identity label protects subsequent pods.
        }
        Ok(())
    }
}
