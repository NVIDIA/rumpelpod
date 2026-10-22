// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;

use indoc::indoc;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use rumpelpod::config::{ContainerEngine, Host};
use rumpelpod::daemon::db;
use rumpelpod::CommandExt;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

const PRE_COMPOSE_SCHEMA_SQL: &str = indoc! {"
    CREATE TABLE pods (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo_path TEXT NOT NULL,
        name TEXT NOT NULL,
        host TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'initializing',
        on_create_ran INTEGER NOT NULL DEFAULT 0,
        post_create_ran INTEGER NOT NULL DEFAULT 0,
        claude_config_copied INTEGER NOT NULL DEFAULT 0,
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

const PRE_COMPOSE_SCHEMA_HASH: &str =
    "ec97b2b08dddd29a2db5e026fb8bdaf2f2458743a797d713dcf72ad98b40d80e";

#[test]
fn pre_compose_database_preserves_data_and_accepts_new_pods() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    write_test_devcontainer(&repo, "", ",\n    \"forwardPorts\": [9123]");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let db_dir = home.path().join("state/rumpelpod");
    fs::create_dir_all(&db_dir).expect("create legacy database directory");
    let db_path = db_dir.join("db.sqlite");
    let conn = Connection::open(&db_path).expect("create legacy database");
    conn.execute_batch(PRE_COMPOSE_SCHEMA_SQL)
        .expect("initialize legacy schema");
    let mut hasher = Sha256::new();
    hasher.update(PRE_COMPOSE_SCHEMA_SQL);
    let schema_hash = hex::encode(hasher.finalize());
    assert_eq!(schema_hash, PRE_COMPOSE_SCHEMA_HASH);
    conn.execute(
        "INSERT INTO db_meta (key, value) VALUES ('schema_version', ?)",
        [&schema_hash],
    )
    .expect("record legacy schema version");
    conn.execute(
        "INSERT INTO db_meta (key, value) VALUES ('migration_sentinel', 'preserved')",
        [],
    )
    .expect("record migration sentinel");
    conn.execute(
        "INSERT INTO pods
            (repo_path, name, host, status, token, image, devcontainer_json,
             local_env, created_at, updated_at)
         VALUES ('/legacy/repo', 'legacy-pod', '\"Localhost\"', 'error',
                 'legacy-token', 'legacy-image', '{}', '[]',
                 '2000-01-01T00:00:00Z', '2000-01-01T00:00:00Z')",
        [],
    )
    .expect("insert legacy pod");
    let legacy_pod_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO forwarded_ports (pod_id, container_port, local_port, label)
         VALUES (?, 8080, 60000, 'legacy')",
        [legacy_pod_id],
    )
    .expect("insert legacy forwarded port");
    drop(conn);

    let mut daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "new-pod", "--", "true"])
        .success()
        .expect("create pod after database migration");

    let conn = Connection::open(&db_path).expect("open migrated database");
    let sentinel: String = conn
        .query_row(
            "SELECT value FROM db_meta WHERE key = 'migration_sentinel'",
            [],
            |row| row.get(0),
        )
        .expect("read migration sentinel");
    assert_eq!(sentinel, "preserved");

    let legacy_compose: (String, String) = conn
        .query_row(
            "SELECT agent_service, compose_config FROM pods WHERE name = 'legacy-pod'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read migrated legacy pod");
    assert_eq!(legacy_compose, (String::new(), String::new()));
    let legacy_port: (String, u16, u16) = conn
        .query_row(
            "SELECT service, container_port, local_port FROM forwarded_ports
             WHERE pod_id = ?",
            [legacy_pod_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read migrated legacy forwarded port");
    assert_eq!(legacy_port, (String::new(), 8080, 60000));
    let forwarded_port_columns: Vec<String> = conn
        .prepare("PRAGMA table_info(forwarded_ports)")
        .expect("prepare forwarded port schema query")
        .query_map([], |row| row.get(1))
        .expect("query forwarded port schema")
        .collect::<Result<_, _>>()
        .expect("read forwarded port schema");
    assert_eq!(
        forwarded_port_columns,
        [
            "id",
            "pod_id",
            "container_port",
            "local_port",
            "label",
            "service"
        ]
    );

    let new_pod_compose: (String, String) = conn
        .query_row(
            "SELECT agent_service, compose_config FROM pods WHERE name = 'new-pod'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read newly created pod");
    assert_eq!(new_pod_compose, (String::new(), String::new()));
    let new_port_service: String = conn
        .query_row(
            "SELECT forwarded_ports.service
             FROM forwarded_ports
             JOIN pods ON pods.id = forwarded_ports.pod_id
             WHERE pods.name = 'new-pod'",
            [],
            |row| row.get(0),
        )
        .expect("read newly created forwarded port");
    assert_eq!(new_port_service, "");
    drop(conn);

    daemon.kill();
    drop(daemon);
    let daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["enter", "new-pod", "--", "true"])
        .success()
        .expect("reopen migrated database and reconnect to new pod");
}

const PRE_ACTIVITY_SCHEMA_SQL: &str = indoc! {"
    CREATE TABLE pods (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo_path TEXT NOT NULL,
        name TEXT NOT NULL,
        host TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'initializing',
        on_create_ran INTEGER NOT NULL DEFAULT 0,
        post_create_ran INTEGER NOT NULL DEFAULT 0,
        claude_config_copied INTEGER NOT NULL DEFAULT 0,
        token TEXT NOT NULL,
        image TEXT NOT NULL,
        devcontainer_json TEXT NOT NULL,
        agent_service TEXT NOT NULL,
        compose_config TEXT NOT NULL,
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
        service TEXT NOT NULL,
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

const PRE_ACTIVITY_SCHEMA_HASH: &str =
    "6a7f9bd88d8806b6478f319de1854599504a3df0b02349ed5d10faab621ac337";

#[test]
fn pre_activity_database_preserves_existing_pods_and_ports() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(PRE_ACTIVITY_SCHEMA_SQL).unwrap();
    assert_eq!(
        hex::encode(Sha256::digest(PRE_ACTIVITY_SCHEMA_SQL)),
        PRE_ACTIVITY_SCHEMA_HASH
    );
    conn.execute(
        "INSERT INTO db_meta VALUES ('schema_version', ?)",
        [PRE_ACTIVITY_SCHEMA_HASH],
    )
    .unwrap();
    conn.execute_batch(indoc! {r#"
        INSERT INTO db_meta VALUES ('sentinel', 'preserved');
        INSERT INTO pods (
            id, repo_path, name, host, status, on_create_ran, post_create_ran,
            claude_config_copied, token, image, devcontainer_json, agent_service,
            compose_config, local_env, created_at, updated_at
        ) VALUES (
            42, '/repo', 'existing', '"Localhost"', 'ready', 1, 1, 1,
            'existing-token', 'existing-image', '{"remoteUser":"dev"}', 'app',
            '{"services":{"app":{"image":"existing-image"}}}', '["MODE=test"]',
            '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z'
        );
        INSERT INTO forwarded_ports VALUES (7, 42, 'db', 5432, 65432, 'database');
    "#})
        .unwrap();
    drop(conn);

    // Reopening twice also checks that migration records the new schema.
    for _ in 0..2 {
        let conn = db::open_db(&path).unwrap();
        let pod = db::get_pod(&conn, std::path::Path::new("/repo"), "existing")
            .unwrap()
            .unwrap();
        assert_eq!(i64::from(pod.id), 42);
        assert_eq!(pod.status, db::PodStatus::Ready);
        assert_eq!(pod.token, "existing-token");
        assert_eq!(pod.image, "existing-image");
        assert_eq!(pod.agent_service, "app");
        assert!(pod.compose_config.contains("existing-image"));
        assert_eq!(pod.local_env, "[\"MODE=test\"]");
        assert!(!pod.agent_processing);
        let lifecycle: (bool, bool, bool) = conn.query_row(
            "SELECT on_create_ran, post_create_ran, claude_config_copied FROM pods WHERE id = 42",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(lifecycle, (true, true, true));
        let port: (String, u16, u16, String) = conn.query_row(
            "SELECT service, container_port, local_port, label FROM forwarded_ports WHERE pod_id = 42",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap();
        assert_eq!(port, ("db".into(), 5432, 65432, "database".into()));
        let sentinel: String = conn
            .query_row(
                "SELECT value FROM db_meta WHERE key = 'sentinel'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sentinel, "preserved");
    }
}

#[test]
fn daemon_restore_failure_keeps_recovery_commands_available() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let path = home.path().join("state/rumpelpod/db.sqlite");
    let conn = db::open_db(&path).unwrap();
    let id = db::create_pod(
        &conn,
        repo.path(),
        "broken-config",
        &Host::Localhost {
            engine: ContainerEngine::Docker,
        },
        "token",
        "image",
        "{}",
        "[]",
    )
    .unwrap();
    db::update_pod_status(&conn, id, db::PodStatus::Ready).unwrap();
    db::record_agent_activity(&conn, repo.path(), "broken-config", "token", true).unwrap();
    drop(conn);
    fs::write(repo.path().join(".rumpelpod.json"), "{ invalid").unwrap();

    let daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["delete", "--forget", "broken-config"])
        .success()
        .expect("a broken processing pod must not prevent recovery commands");
    let conn = db::open_db(&path).unwrap();
    assert!(db::get_pod(&conn, repo.path(), "broken-config")
        .unwrap()
        .is_none());
}
