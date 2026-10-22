// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::UnixListener;
use tokio::sync::watch;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use super::{run_codex_monitor, CodexState, CodexThreads};

fn status(threads: &mut CodexThreads, id: &str, status: Value) -> Option<CodexState> {
    threads
        .notification(&json!({
            "method": "thread/status/changed",
            "params": {"threadId": id, "status": status},
        }))
        .unwrap();
    threads.state()
}

#[test]
fn codex_status_keeps_working_until_all_threads_are_idle() {
    let mut threads = CodexThreads::default();
    let active = json!({"type": "active", "activeFlags": []});
    let idle = json!({"type": "idle"});
    assert_eq!(
        status(&mut threads, "parent", active.clone()),
        Some(CodexState::Processing)
    );
    assert_eq!(
        status(&mut threads, "child", active.clone()),
        Some(CodexState::Processing)
    );
    assert_eq!(
        status(&mut threads, "parent", idle.clone()),
        Some(CodexState::Processing)
    );
    assert_eq!(
        status(&mut threads, "child", idle.clone()),
        Some(CodexState::Idle)
    );

    assert_eq!(
        status(&mut threads, "parent", active),
        Some(CodexState::Processing)
    );
    assert_eq!(
        status(&mut threads, "child", idle.clone()),
        Some(CodexState::Processing)
    );
    assert_eq!(status(&mut threads, "parent", idle), Some(CodexState::Idle));
}

#[test]
fn codex_status_distinguishes_user_waits_errors_and_unloaded_threads() {
    let mut threads = CodexThreads::default();
    for flag in ["waitingOnApproval", "waitingOnUserInput"] {
        assert_eq!(
            status(
                &mut threads,
                "parent",
                json!({"type": "active", "activeFlags": [flag]})
            ),
            Some(CodexState::Idle)
        );
    }
    assert_eq!(
        status(
            &mut threads,
            "child",
            json!({"type": "active", "activeFlags": []})
        ),
        Some(CodexState::Processing)
    );
    assert_eq!(
        status(&mut threads, "parent", json!({"type": "systemError"})),
        Some(CodexState::Processing)
    );
    assert_eq!(
        status(&mut threads, "child", json!({"type": "idle"})),
        Some(CodexState::Error)
    );
    assert_eq!(
        status(&mut threads, "parent", json!({"type": "notLoaded"})),
        Some(CodexState::Idle)
    );
    assert_eq!(
        status(&mut threads, "child", json!({"type": "notLoaded"})),
        None
    );
}

#[test]
fn codex_status_rejects_unknown_protocol_states() {
    let mut threads = CodexThreads::default();
    for value in [
        json!({"type": "newState"}),
        json!({"type": "active", "activeFlags": ["newFlag"]}),
        json!({"type": "active"}),
    ] {
        assert!(threads
            .notification(&json!({
                "method": "thread/status/changed",
                "params": {"threadId": "parent", "status": value},
            }))
            .is_err());
    }
}

#[tokio::test]
async fn codex_status_monitor_restores_loaded_threads_and_handles_snapshot_races() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("codex.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, mut rx) = watch::channel(None);
    let monitor = tokio::spawn(async move { run_codex_monitor(&tx, &socket).await });
    let (stream, _) = listener.accept().await.unwrap();
    let mut ws = accept_async(stream).await.unwrap();
    let init: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(init["method"], "initialize");
    ws.send(Message::Text(
        json!({"id": init["id"], "result": {}}).to_string().into(),
    ))
    .await
    .unwrap();
    let initialized: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(initialized["method"], "initialized");

    // Pagination and metadata-only reads must include both the parent and its
    // child, even though neither emits a new status event after reconnecting.
    for (id, cursor, snapshot) in [
        (
            "parent",
            Some("page-2"),
            json!({"type": "active", "activeFlags": []}),
        ),
        ("child", None, json!({"type": "idle"})),
    ] {
        let request: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(request["method"], "thread/loaded/list");
        let expected_cursor = if id == "parent" { None } else { Some("page-2") };
        assert_eq!(request["params"]["cursor"], json!(expected_cursor));
        ws.send(Message::Text(
            json!({
                "id": request["id"], "result": {"data": [id], "nextCursor": cursor}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        let request: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(request["method"], "thread/read");
        assert_eq!(
            request["params"],
            json!({"threadId": id, "includeTurns": false})
        );
        if id == "child" {
            // A child starts working while its older idle snapshot is in flight.
            ws.send(Message::Text(
                json!({
                    "method": "thread/status/changed",
                    "params": {"threadId": "child", "status": {"type": "active", "activeFlags": []}}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                json!({
                    "method": "thread/status/changed",
                    "params": {"threadId": "parent", "status": {"type": "idle"}}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        }
        ws.send(Message::Text(
            json!({
                "id": request["id"], "result": {"thread": {"id": id, "status": snapshot}}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        rx.wait_for(|state| *state == Some(CodexState::Processing)),
    )
    .await
    .unwrap()
    .unwrap();
    ws.send(Message::Text(
        json!({
            "method": "thread/status/changed",
            "params": {"threadId": "child", "status": {"type": "idle"}}
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        rx.wait_for(|state| *state == Some(CodexState::Idle)),
    )
    .await
    .unwrap()
    .unwrap();
    ws.close(None).await.unwrap();
    monitor.await.unwrap().unwrap();
}
