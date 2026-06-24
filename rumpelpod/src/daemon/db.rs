// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! SQLite database for persisting pod metadata.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use indoc::indoc;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::config::Host;

/// Strongly-typed wrapper for pod database IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PodId(i64);

impl From<i64> for PodId {
    fn from(id: i64) -> Self {
        PodId(id)
    }
}

impl From<PodId> for i64 {
    fn from(id: PodId) -> Self {
        id.0
    }
}

/// Status of a pod in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodStatus {
    /// Pod is being initialized (container creation, git setup, etc.)
    Initializing,
    /// Pod is fully initialized and ready
    Ready,
    /// Pod initialization failed
    Error,
    /// Pod container is being stopped in the background
    Stopping,
    /// Pod container is being deleted in the background
    Deleting,
    /// Background deletion failed after all retries
    DeleteFailed,
}

impl PodStatus {
    fn as_str(&self) -> &'static str {
        match self {
            PodStatus::Initializing => "initializing",
            PodStatus::Ready => "ready",
            PodStatus::Error => "error",
            PodStatus::Stopping => "stopping",
            PodStatus::Deleting => "deleting",
            PodStatus::DeleteFailed => "delete_failed",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "initializing" => Some(PodStatus::Initializing),
            "ready" => Some(PodStatus::Ready),
            "error" => Some(PodStatus::Error),
            "stopping" => Some(PodStatus::Stopping),
            "deleting" => Some(PodStatus::Deleting),
            "delete_failed" => Some(PodStatus::DeleteFailed),
            _ => None,
        }
    }
}

/// Information about a pod from the database.
#[derive(Debug, Clone)]
pub struct PodRecord {
    pub id: PodId,
    pub repo_path: String,
    pub name: String,
    /// The host where the pod runs: "localhost" or "ssh://user@host".
    pub host: String,
    pub status: PodStatus,
    pub token: String,
    /// Final image tag passed to create_container. Stored so a fork can
    /// reuse the exact same image without re-resolving devcontainer.json
    /// (which may have changed on the host since pod creation).
    pub image: String,
    /// Raw devcontainer.json source captured at pod creation. Forks parse
    /// this for mounts/ports/user instead of re-reading the host file.
    pub devcontainer_json: String,
    /// JSON array of "KEY=VALUE" strings -- the --local-env arguments
    /// the pod was created with.
    pub local_env: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Get the database path.
/// Uses $XDG_STATE_HOME/rumpelpod/db.sqlite
pub fn db_path() -> Result<PathBuf> {
    let state_base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .expect("could not determine home directory")
                .join(".local/state")
        });

    Ok(state_base.join("rumpelpod").join("db.sqlite"))
}

const SCHEMA_SQL: &str = indoc! {"
    CREATE TABLE pods (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo_path TEXT NOT NULL,
        name TEXT NOT NULL,
        host TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'initializing',
        on_create_ran INTEGER NOT NULL DEFAULT 0,
        post_create_ran INTEGER NOT NULL DEFAULT 0,
        claude_config_copied INTEGER NOT NULL DEFAULT 0,
        pi_config_copied INTEGER NOT NULL DEFAULT 0,
        token TEXT NOT NULL,
        image TEXT NOT NULL,
        devcontainer_json TEXT NOT NULL,
        local_env TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(repo_path, name)
    );

    CREATE INDEX idx_pods_lookup
        ON pods(repo_path, name);

    CREATE TABLE forwarded_ports (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        pod_id INTEGER NOT NULL REFERENCES pods(id) ON DELETE CASCADE,
        container_port INTEGER NOT NULL,
        local_port INTEGER NOT NULL,
        label TEXT NOT NULL DEFAULT ''
    );

    CREATE INDEX idx_forwarded_ports_pod
        ON forwarded_ports(pod_id);

    CREATE UNIQUE INDEX idx_forwarded_ports_local
        ON forwarded_ports(local_port);

    CREATE TABLE db_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
"};

fn get_schema_hash() -> String {
    let mut hasher = Sha256::new();
    hasher.update(SCHEMA_SQL);
    hex::encode(hasher.finalize())
}

/// Open a database connection, creating the database and tables if needed.
pub fn open_db(path: &Path) -> Result<Connection> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            let parent = parent.display();
            format!("failed to create directory {parent}")
        })?;
    }

    if !path.exists() {
        return create_and_init_db(path);
    }

    let conn = Connection::open(path).with_context(|| {
        let path = path.display();
        format!("failed to open database at {path}")
    })?;

    // Enable foreign key enforcement
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .context("failed to enable foreign keys")?;

    // Check version
    let current_hash = get_schema_hash();
    let stored_hash: Result<String, _> = conn.query_row(
        "SELECT value FROM db_meta WHERE key = 'schema_version'",
        [],
        |row| row.get(0),
    );

    match stored_hash {
        Ok(hash) => {
            if hash != current_hash {
                let path = path.display();
                return Err(anyhow::anyhow!(
                    "database schema mismatch\n\
                    expected hash: {current_hash}\n\
                    found hash:    {hash}\n\
                    \n\
                    delete the database file to start over:\n\
                    rm {path}"
                ));
            }
        }
        Err(rusqlite::Error::SqliteFailure(_, _)) | Err(rusqlite::Error::QueryReturnedNoRows) => {
            // Treat missing table or missing row as "no version".
            // Close connection, delete file, and start over.
            drop(conn);
            std::fs::remove_file(path).with_context(|| {
                let path = path.display();
                format!("failed to remove outdated database at {path}")
            })?;
            return create_and_init_db(path);
        }
        Err(e) => {
            // Other errors (e.g. database corruption)
            return Err(e).context("failed to read schema version from database");
        }
    }

    Ok(conn)
}

fn create_and_init_db(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path).with_context(|| {
        let path = path.display();
        format!("failed to open new database at {path}")
    })?;

    // Enable foreign key enforcement
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .context("failed to enable foreign keys")?;

    let tx = conn.transaction()?;

    tx.execute_batch(SCHEMA_SQL)
        .context("failed to initialize database schema")?;

    let hash = get_schema_hash();
    tx.execute(
        "INSERT INTO db_meta (key, value) VALUES ('schema_version', ?)",
        [&hash],
    )
    .context("failed to insert schema version")?;

    tx.commit()?;

    Ok(conn)
}

// --- Pod functions ---

/// Create a new pod record with status "initializing".
///
/// Returns the ID of the new pod.
/// Returns an error if a pod with this repo_path and name already exists.
#[allow(clippy::too_many_arguments)]
pub fn create_pod(
    conn: &Connection,
    repo_path: &Path,
    name: &str,
    host: &Host,
    token: &str,
    image: &str,
    devcontainer_json: &str,
    local_env: &str,
) -> Result<PodId> {
    let now = Utc::now().to_rfc3339();
    let repo_path_str = repo_path.to_string_lossy();
    let host_json = serde_json::to_string(host).context("failed to serialize host")?;

    conn.execute(
        "INSERT INTO pods
            (repo_path, name, host, status, token, image, devcontainer_json,
             local_env, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            repo_path_str,
            name,
            host_json,
            PodStatus::Initializing.as_str(),
            token,
            image,
            devcontainer_json,
            local_env,
            now,
            now
        ],
    )
    .context("failed to insert pod")?;

    Ok(PodId(conn.last_insert_rowid()))
}

/// Update the status of a pod.
pub fn update_pod_status(conn: &Connection, id: PodId, status: PodStatus) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE pods SET status = ?, updated_at = ? WHERE id = ?",
        rusqlite::params![status.as_str(), now, i64::from(id)],
    )
    .context("failed to update pod status")?;
    Ok(())
}

fn row_to_pod_record(row: &rusqlite::Row) -> rusqlite::Result<PodRecord> {
    let status_str: String = row.get(4)?;
    let status = PodStatus::from_str(&status_str).unwrap_or(PodStatus::Error);
    Ok(PodRecord {
        id: PodId(row.get(0)?),
        repo_path: row.get(1)?,
        name: row.get(2)?,
        host: row.get(3)?,
        status,
        token: row.get(5)?,
        image: row.get(6)?,
        devcontainer_json: row.get(7)?,
        local_env: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

/// Get a pod by repo_path and name.
pub fn get_pod(conn: &Connection, repo_path: &Path, name: &str) -> Result<Option<PodRecord>> {
    let repo_path_str = repo_path.to_string_lossy();

    let mut stmt = conn
        .prepare(
            "SELECT id, repo_path, name, host, status, token, image, devcontainer_json, local_env, created_at, updated_at
             FROM pods WHERE repo_path = ? AND name = ?",
        )
        .context("failed to prepare query")?;

    let mut rows = stmt
        .query_map(rusqlite::params![repo_path_str, name], row_to_pod_record)
        .context("failed to query pod")?;

    match rows.next() {
        Some(row) => Ok(Some(row.context("failed to read pod")?)),
        None => Ok(None),
    }
}

/// Get a pod by ID.
#[cfg(test)]
pub fn get_pod_by_id(conn: &Connection, id: PodId) -> Result<Option<PodRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, repo_path, name, host, status, token, image, devcontainer_json, local_env, created_at, updated_at
             FROM pods WHERE id = ?",
        )
        .context("failed to prepare query")?;

    let mut rows = stmt
        .query_map(rusqlite::params![i64::from(id)], row_to_pod_record)
        .context("failed to query pod")?;

    match rows.next() {
        Some(row) => Ok(Some(row.context("failed to read pod")?)),
        None => Ok(None),
    }
}

/// Look up a pod by its token.
pub fn get_pod_by_token(conn: &Connection, token: &str) -> Result<Option<PodRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, repo_path, name, host, status, token, image, devcontainer_json, local_env, created_at, updated_at
             FROM pods WHERE token = ?",
        )
        .context("failed to prepare query")?;

    let mut rows = stmt
        .query_map(rusqlite::params![token], row_to_pod_record)
        .context("failed to query pod by token")?;

    match rows.next() {
        Some(row) => Ok(Some(row.context("failed to read pod")?)),
        None => Ok(None),
    }
}

/// List all pods with the given status, across all repos.
pub fn list_pods_by_status(conn: &Connection, status: PodStatus) -> Result<Vec<PodRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, repo_path, name, host, status, token, image, devcontainer_json, local_env, created_at, updated_at
             FROM pods WHERE status = ?
             ORDER BY repo_path ASC, name ASC",
        )
        .context("failed to prepare query")?;

    let rows = stmt
        .query_map(rusqlite::params![status.as_str()], row_to_pod_record)
        .context("failed to query pods by status")?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.context("failed to read pod row")?);
    }

    Ok(result)
}

/// List all pods for a given repo path.
pub fn list_pods(conn: &Connection, repo_path: &Path) -> Result<Vec<PodRecord>> {
    let repo_path_str = repo_path.to_string_lossy();

    let mut stmt = conn
        .prepare(
            "SELECT id, repo_path, name, host, status, token, image, devcontainer_json, local_env, created_at, updated_at
             FROM pods WHERE repo_path = ?
             ORDER BY name ASC",
        )
        .context("failed to prepare query")?;

    let rows = stmt
        .query_map(rusqlite::params![repo_path_str], row_to_pod_record)
        .context("failed to query pods")?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.context("failed to read pod row")?);
    }

    Ok(result)
}

/// List all pods across all repos.
pub fn list_all_pods(conn: &Connection) -> Result<Vec<PodRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, repo_path, name, host, status, token, image, devcontainer_json, local_env, created_at, updated_at
             FROM pods
             ORDER BY repo_path ASC, name ASC",
        )
        .context("failed to prepare query")?;

    let rows = stmt
        .query_map([], row_to_pod_record)
        .context("failed to query all pods")?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.context("failed to read pod row")?);
    }

    Ok(result)
}

/// Delete a pod and all rows that reference it.
pub fn delete_pod(conn: &Connection, repo_path: &Path, name: &str) -> Result<bool> {
    let repo_path_str = repo_path.to_string_lossy();
    let count = conn
        .execute(
            "DELETE FROM pods WHERE repo_path = ? AND name = ?",
            rusqlite::params![repo_path_str, name],
        )
        .context("failed to delete pod")?;
    Ok(count > 0)
}

/// Check whether Claude Code config files have been copied into this pod.
pub fn has_claude_config_copied(conn: &Connection, id: PodId) -> Result<bool> {
    let copied: bool = conn
        .query_row(
            "SELECT claude_config_copied FROM pods WHERE id = ?",
            rusqlite::params![i64::from(id)],
            |row| row.get(0),
        )
        .context("failed to query claude_config_copied")?;
    Ok(copied)
}

/// Mark Claude Code config files as having been copied into this pod.
pub fn mark_claude_config_copied(conn: &Connection, id: PodId) -> Result<()> {
    conn.execute(
        "UPDATE pods SET claude_config_copied = 1 WHERE id = ?",
        rusqlite::params![i64::from(id)],
    )
    .context("failed to mark claude_config_copied")?;
    Ok(())
}

/// Check whether pi config files have been copied into this pod.
pub fn has_pi_config_copied(conn: &Connection, id: PodId) -> Result<bool> {
    let copied: bool = conn
        .query_row(
            "SELECT pi_config_copied FROM pods WHERE id = ?",
            rusqlite::params![i64::from(id)],
            |row| row.get(0),
        )
        .context("Failed to query pi_config_copied")?;
    Ok(copied)
}

/// Mark pi config files as having been copied into this pod.
pub fn mark_pi_config_copied(conn: &Connection, id: PodId) -> Result<()> {
    conn.execute(
        "UPDATE pods SET pi_config_copied = 1 WHERE id = ?",
        rusqlite::params![i64::from(id)],
    )
    .context("Failed to mark pi_config_copied")?;
    Ok(())
}

pub struct ForwardedPort {
    pub container_port: u16,
    pub local_port: u16,
    pub label: String,
}

pub fn insert_forwarded_port(
    conn: &Connection,
    pod_id: PodId,
    container_port: u16,
    local_port: u16,
    label: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO forwarded_ports (pod_id, container_port, local_port, label)
         VALUES (?, ?, ?, ?)",
        rusqlite::params![pod_id.0, container_port, local_port, label],
    )
    .context("inserting forwarded port")?;
    Ok(())
}

pub fn list_forwarded_ports(conn: &Connection, pod_id: PodId) -> Result<Vec<ForwardedPort>> {
    let mut stmt = conn
        .prepare(
            "SELECT container_port, local_port, label
             FROM forwarded_ports WHERE pod_id = ?
             ORDER BY container_port",
        )
        .context("preparing forwarded ports query")?;

    let ports = stmt
        .query_map(rusqlite::params![pod_id.0], |row| {
            Ok(ForwardedPort {
                container_port: row.get(0)?,
                local_port: row.get(1)?,
                label: row.get(2)?,
            })
        })
        .context("querying forwarded ports")?
        .collect::<Result<Vec<_>, _>>()
        .context("reading forwarded port rows")?;

    Ok(ports)
}

pub fn get_all_allocated_local_ports(conn: &Connection) -> Result<Vec<u16>> {
    let mut stmt = conn
        .prepare("SELECT local_port FROM forwarded_ports")
        .context("preparing allocated ports query")?;

    let ports = stmt
        .query_map([], |row| row.get(0))
        .context("querying allocated ports")?
        .collect::<Result<Vec<_>, _>>()
        .context("reading allocated port rows")?;

    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (TempDir, Connection) {
        let temp_dir = TempDir::with_prefix("rumpelpod-db-test-").unwrap();
        let db_path = temp_dir.path().join("test.db");
        let conn = open_db(&db_path).unwrap();
        (temp_dir, conn)
    }

    // --- Pod tests ---

    #[test]
    fn test_create_and_get_pod() {
        let (_temp_dir, conn) = test_db();

        let repo_path = PathBuf::from("/home/user/project");

        let id = create_pod(
            &conn,
            &repo_path,
            "dev",
            &Host::Localhost,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();
        assert!(i64::from(id) > 0);

        let pod = get_pod(&conn, &repo_path, "dev").unwrap().unwrap();
        assert_eq!(pod.id, id);
        assert_eq!(pod.name, "dev");
        assert_eq!(pod.host, serde_json::to_string(&Host::Localhost).unwrap());
        assert_eq!(pod.status, PodStatus::Initializing);
    }

    #[test]
    fn test_create_pod_with_host() {
        let (_temp_dir, conn) = test_db();

        let repo_path = PathBuf::from("/home/user/project");

        let ssh_host = Host::Ssh {
            ssh_destination: "user@host".to_string(),
        };
        let id = create_pod(
            &conn,
            &repo_path,
            "remote",
            &ssh_host,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();

        let pod = get_pod(&conn, &repo_path, "remote").unwrap().unwrap();
        assert_eq!(pod.id, id);
        assert_eq!(pod.host, serde_json::to_string(&ssh_host).unwrap());
    }

    #[test]
    fn test_create_pod_duplicate_fails() {
        let (_temp_dir, conn) = test_db();

        let repo_path = PathBuf::from("/home/user/project");

        create_pod(
            &conn,
            &repo_path,
            "dev",
            &Host::Localhost,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();

        let result = create_pod(
            &conn,
            &repo_path,
            "dev",
            &Host::Localhost,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_update_pod_status() {
        let (_temp_dir, conn) = test_db();

        let repo_path = PathBuf::from("/home/user/project");

        let id = create_pod(
            &conn,
            &repo_path,
            "dev",
            &Host::Localhost,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();

        let pod = get_pod(&conn, &repo_path, "dev").unwrap().unwrap();
        assert_eq!(pod.status, PodStatus::Initializing);

        update_pod_status(&conn, id, PodStatus::Ready).unwrap();
        let pod = get_pod(&conn, &repo_path, "dev").unwrap().unwrap();
        assert_eq!(pod.status, PodStatus::Ready);

        update_pod_status(&conn, id, PodStatus::Error).unwrap();
        let pod = get_pod(&conn, &repo_path, "dev").unwrap().unwrap();
        assert_eq!(pod.status, PodStatus::Error);
    }

    #[test]
    fn test_list_pods() {
        let (_temp_dir, conn) = test_db();

        let repo_path = PathBuf::from("/home/user/project");

        let ssh_host = Host::Ssh {
            ssh_destination: "remote".to_string(),
        };
        create_pod(
            &conn,
            &repo_path,
            "dev",
            &Host::Localhost,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();
        create_pod(
            &conn,
            &repo_path,
            "test",
            &ssh_host,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();

        let pods = list_pods(&conn, &repo_path).unwrap();
        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].name, "dev");
        assert_eq!(pods[1].name, "test");
        assert_eq!(pods[1].host, serde_json::to_string(&ssh_host).unwrap());
    }

    #[test]
    fn test_list_pods_filters_by_repo() {
        let (_temp_dir, conn) = test_db();

        let repo1 = PathBuf::from("/home/user/project1");
        let repo2 = PathBuf::from("/home/user/project2");

        create_pod(
            &conn,
            &repo1,
            "dev",
            &Host::Localhost,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();
        create_pod(
            &conn,
            &repo2,
            "dev",
            &Host::Localhost,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();

        let repo1_pods = list_pods(&conn, &repo1).unwrap();
        assert_eq!(repo1_pods.len(), 1);
        assert_eq!(repo1_pods[0].repo_path, repo1.to_string_lossy());

        let repo2_pods = list_pods(&conn, &repo2).unwrap();
        assert_eq!(repo2_pods.len(), 1);
        assert_eq!(repo2_pods[0].repo_path, repo2.to_string_lossy());
    }

    #[test]
    fn test_delete_pod() {
        let (_temp_dir, conn) = test_db();

        let repo_path = PathBuf::from("/home/user/project");

        create_pod(
            &conn,
            &repo_path,
            "dev",
            &Host::Localhost,
            "test-token",
            "img:tag",
            "{}",
            "[]",
        )
        .unwrap();

        let deleted = delete_pod(&conn, &repo_path, "dev").unwrap();
        assert!(deleted);

        let pod = get_pod(&conn, &repo_path, "dev").unwrap();
        assert!(pod.is_none());

        let deleted = delete_pod(&conn, &repo_path, "dev").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn test_db_versioning_recreation() {
        let temp_dir = TempDir::with_prefix("rumpelpod-db-test-versioning-").unwrap();
        let db_path = temp_dir.path().join("test.db");

        // 1. Create fresh DB
        {
            let conn = open_db(&db_path).unwrap();
            // Verify version exists
            let hash: String = conn
                .query_row(
                    "SELECT value FROM db_meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(hash, get_schema_hash());
        }

        // 2. Corrupt DB (remove version)
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute("DELETE FROM db_meta WHERE key = 'schema_version'", [])
                .unwrap();
        }

        // 3. Open again - should detect missing version and re-create
        {
            let conn = open_db(&db_path).unwrap();
            let hash: String = conn
                .query_row(
                    "SELECT value FROM db_meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(hash, get_schema_hash());

            // Verify it was re-created (tables should be empty if we had added data,
            // but here we just check it opened successfully and has version)
        }
    }

    #[test]
    fn test_db_versioning_mismatch() {
        let temp_dir = TempDir::with_prefix("rumpelpod-db-test-mismatch-").unwrap();
        let db_path = temp_dir.path().join("test.db");

        // 1. Create DB with wrong version hash manually
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(SCHEMA_SQL).unwrap();
            conn.execute(
                "INSERT INTO db_meta (key, value) VALUES ('schema_version', 'wrong_hash')",
                [],
            )
            .unwrap();
        }

        // 2. Open should fail
        let result = open_db(&db_path);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("database schema mismatch"));
        assert!(err.to_string().contains("rm "));
    }
}
