// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use indoc::indoc;
use rumpelpod::CommandExt;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::claude::common::{setup_claude_test_repo, ClaudeSession};
use crate::codex::common::{setup_codex_test_repo, CodexSession};
use crate::common::{create_commit, launch_pod_via_daemon, pod_command, TestDaemon, TestRepo};

fn prepare_workers(repo: &TestRepo, instructions: &str) {
    fs::write(repo.path().join("delegation.txt"), instructions).unwrap();
    // FIFOs keep the workers alive until the assertions have observed them.
    // Their output is independent of scheduling and safe to replay from cache.
    for worker in ["first", "second"] {
        fs::write(
            repo.path().join(format!("status-{worker}.sh")),
            format!("worker={worker}\n")
                + indoc! {r#"
                set -eu
                gate="/tmp/status-worker-$worker"
                mkfifo "$gate.release"
                touch "$gate.ready"
                read -r token < "$gate.release"
                test "$token" = release
                printf '%s complete\n' "$worker"
            "#},
        )
        .unwrap();
    }
    Command::new("git")
        .current_dir(repo.path())
        .args([
            "add",
            "delegation.txt",
            "status-first.sh",
            "status-second.sh",
        ])
        .success()
        .unwrap();
    create_commit(repo.path(), "Keep delegated work open until observed");
}

fn pod_shell(repo: &TestRepo, daemon: &TestDaemon, script: &str) -> String {
    let bytes = pod_command(repo, daemon)
        .args(["enter", "test", "--", "sh", "-c", script])
        .success()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

fn wait_for(description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if condition() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out: {description}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn list(repo: &TestRepo, daemon: &TestDaemon) -> String {
    String::from_utf8(pod_command(repo, daemon).arg("list").success().unwrap()).unwrap()
}

fn assert_processing(repo: &TestRepo, daemon: &TestDaemon, column: &str) {
    // The gate stays closed throughout this observation period. Requiring a
    // stable status also rules out a stale host value while SSE catches up.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let status = list(repo, daemon);
        assert!(status.contains(column), "{status}");
        assert!(status.contains("processing"), "{status}");
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn claude_status_with_real_subagents() {
    let (home, repo, _executor, daemon) = setup_claude_test_repo();
    prepare_workers(
        &repo,
        indoc! {"
            Spawn exactly two background subagents using the Agent tool with
            run_in_background=true and model=haiku. Tell the first to run
            `sh status-first.sh` and the second `sh status-second.sh`.
            Each worker must run only that command, wait for it to finish, then
            reply with its output. Never read or modify the scripts or gate files.
            After launching both workers, immediately reply 'delegated' and end
            your turn. Do not wait for the workers, read their outputs, or run any
            other tools. When completion notifications arrive, acknowledge them
            briefly without starting more work. This is a status integration test;
            an external test process releases the workers when it is ready.
        "},
    );

    // Let rumpel install its production hooks, then add acknowledgements to
    // those exact commands before starting the session under observation.
    let mut setup = ClaudeSession::spawn(&repo, &daemon, home.path(), "claude-haiku-4-5", &[]);
    setup.wait_for("~/workspace");
    setup.send("/exit");
    setup.wait_for_exit();
    let mut settings: Value = serde_json::from_str(&pod_shell(
        &repo,
        &daemon,
        "cat \"$HOME/.claude/settings.json\"",
    ))
    .unwrap();
    for event in ["Stop", "SubagentStart", "SubagentStop"] {
        let command = settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .expect("production lifecycle hook")
            .to_owned();
        settings["hooks"][event][0]["hooks"][0]["command"] = Value::String(format!(
            "{command} && printf '%s\\n' '{event}' >> /tmp/status-events"
        ));
    }
    let mut upload = pod_command(&repo, &daemon)
        .args([
            "enter",
            "test",
            "--",
            "sh",
            "-c",
            "cat > \"$HOME/.claude/settings.json\"",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    upload
        .stdin
        .take()
        .unwrap()
        .write_all(settings.to_string().as_bytes())
        .unwrap();
    assert!(upload.wait().unwrap().success());

    let mut session = ClaudeSession::spawn(&repo, &daemon, home.path(), "claude-haiku-4-5", &[]);
    session.wait_for("~/workspace");
    session.send("Read delegation.txt and follow its instructions.");
    wait_for(
        "both real Claude workers and the parent's Stop hook",
        || {
            pod_shell(
                &repo,
                &daemon,
                indoc! {"
            if test -f /tmp/status-worker-first.ready &&
               test -f /tmp/status-worker-second.ready &&
               test -f /tmp/status-events; then cat /tmp/status-events; fi
        "},
            )
            .lines()
            .any(|event| event == "Stop")
        },
    );
    assert_processing(&repo, &daemon, "CLAUDE");

    pod_shell(
        &repo,
        &daemon,
        "printf 'release\\n' > /tmp/status-worker-first.release",
    );
    wait_for("first real Claude subagent completion", || {
        pod_shell(&repo, &daemon, "cat /tmp/status-events")
            .lines()
            .filter(|event| *event == "SubagentStop")
            .count()
            == 1
    });
    assert_processing(&repo, &daemon, "CLAUDE");

    pod_shell(
        &repo,
        &daemon,
        "printf 'release\\n' > /tmp/status-worker-second.release",
    );
    wait_for(
        "both real Claude subagents complete and the session idle",
        || {
            pod_shell(&repo, &daemon, "cat /tmp/status-events")
                .lines()
                .filter(|event| *event == "SubagentStop")
                .count()
                == 2
                && list(&repo, &daemon).contains("idle")
        },
    );
}

struct CodexObserver {
    runtime: tokio::runtime::Runtime,
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    observed_ids: BTreeSet<String>,
}

impl CodexObserver {
    fn connect(repo: &TestRepo, daemon: &TestDaemon) -> Self {
        let launch = launch_pod_via_daemon(repo, daemon, "test");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let socket = runtime.block_on(async {
            let mut url = url::Url::parse(&launch.container_url).unwrap();
            let scheme = match url.scheme() {
                "http" => "ws",
                "https" => "wss",
                other => panic!("unexpected pod URL scheme: {other}"),
            };
            url.set_scheme(scheme).unwrap();
            url.set_path("/codex");
            url.set_query(Some("bypass=true"));
            let mut request = url.as_str().into_client_request().unwrap();
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {}", launch.container_token)
                    .parse()
                    .unwrap(),
            );
            tokio_tungstenite::connect_async(request).await.unwrap().0
        });
        let mut observer = Self {
            runtime,
            socket,
            next_id: 0,
            observed_ids: BTreeSet::new(),
        };
        observer.rpc(
            "initialize",
            json!({
                "clientInfo": {"name": "status-regression", "version": "1"}
            }),
        );
        observer
            .runtime
            .block_on(observer.socket.send(Message::Text(
                json!({"method": "initialized"}).to_string().into(),
            )))
            .unwrap();
        observer
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    self.socket
                        .send(Message::Text(
                            json!({"id": id, "method": method, "params": params})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    loop {
                        let message = self
                            .socket
                            .next()
                            .await
                            .expect("Codex observer disconnected")
                            .unwrap();
                        match message {
                            Message::Text(text) => {
                                let mut response: Value = serde_json::from_str(&text).unwrap();
                                if response["id"] == id {
                                    assert!(
                                        response.get("error").is_none(),
                                        "{method}: {response}"
                                    );
                                    return response["result"].take();
                                }
                            }
                            Message::Ping(_) | Message::Pong(_) => {}
                            Message::Close(_) | Message::Binary(_) | Message::Frame(_) => {
                                panic!("unexpected Codex observer message: {message:?}");
                            }
                        }
                    }
                })
                .await
            })
            .unwrap_or_else(|_| panic!("Codex {method} timed out"))
    }

    fn statuses(&mut self) -> BTreeMap<String, String> {
        let threads = self.rpc("thread/loaded/list", json!({}));
        assert!(
            threads["nextCursor"].is_null(),
            "unexpected pagination: {threads}"
        );
        threads["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| {
                let thread = self.rpc(
                    "thread/read",
                    json!({"threadId": id, "includeTurns": false}),
                );
                let identity = thread["thread"]["source"]["subAgent"]["thread_spawn"]["agent_path"]
                    .as_str()
                    .unwrap_or(id.as_str().unwrap())
                    .to_owned();
                if self.observed_ids.insert(identity.clone()) {
                    eprintln!(
                        "Codex thread {id}: source={}, parent={}, threadSource={}",
                        thread["thread"]["source"],
                        thread["thread"]["parentThreadId"],
                        thread["thread"]["threadSource"]
                    );
                }
                (
                    identity,
                    thread["thread"]["status"]["type"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
            })
            .collect()
    }
}

#[test]
fn codex_status_with_real_subagents() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();
    let config_path = home.path().join(".codex/config.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        format!("agents.max_concurrent_threads_per_session = 2\n{config}"),
    )
    .unwrap();
    prepare_workers(
        &repo,
        indoc! {"
        Spawn exactly two subagents with spawn_agent, named status_first and
        status_second (set task_name explicitly). Tell the first to run
        `sh status-first.sh` and the second `sh status-second.sh`.
        Each worker must run only that command with exec_command using
        yield_time_ms=30000, wait for it to finish, then reply with its output.
        Never read or modify the scripts or gate files. After launching both
        workers, immediately reply 'delegated' and end your turn. Do not wait
        for the workers, close them, or run any other tools. When completion
        notifications arrive, acknowledge them briefly without starting more
        work. This is a status integration test; an external test process
        releases the workers when it is ready.
    "},
    );
    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.wait_for_ready_with_timeout(Duration::from_secs(30));
    let mut observer = CodexObserver::connect(&repo, &daemon);
    let initial = observer.statuses();
    assert_eq!(initial.len(), 1, "{initial:?}");
    let parent = initial.keys().next().unwrap().clone();
    session.send("Read delegation.txt and follow its instructions.");
    let mut last_observation = Instant::now();
    wait_for(
        "Codex's final response with two real subagents still working",
        || {
            let statuses = observer.statuses();
            let delegated = statuses.get(&parent).is_some_and(|status| status == "idle");
            if last_observation.elapsed() >= Duration::from_secs(5) {
                eprintln!("Codex threads: {statuses:?}; parent replied: {delegated}");
                last_observation = Instant::now();
            }
            delegated
                && statuses
                    .get("/root/status_first")
                    .is_some_and(|s| s == "active")
                && statuses
                    .get("/root/status_second")
                    .is_some_and(|s| s == "active")
                && pod_shell(
                    &repo,
                    &daemon,
                    indoc! {"
                if test -f /tmp/status-worker-first.ready &&
                   test -f /tmp/status-worker-second.ready; then echo ready; fi
            "},
                )
                .trim()
                    == "ready"
        },
    );
    assert_processing(&repo, &daemon, "CODEX");

    pod_shell(
        &repo,
        &daemon,
        "printf 'release\\n' > /tmp/status-worker-first.release",
    );
    wait_for("one idle Codex subagent while its sibling works", || {
        let statuses = observer.statuses();
        statuses.get(&parent).is_some_and(|s| s == "idle")
            && statuses
                .get("/root/status_first")
                .is_some_and(|s| s == "idle")
            && statuses
                .get("/root/status_second")
                .is_some_and(|s| s == "active")
    });
    assert_processing(&repo, &daemon, "CODEX");

    pod_shell(
        &repo,
        &daemon,
        "printf 'release\\n' > /tmp/status-worker-second.release",
    );
    wait_for(
        "all real Codex threads and the reported status idle",
        || {
            observer.statuses().values().all(|s| s == "idle")
                && list(&repo, &daemon).contains("idle")
        },
    );
}
