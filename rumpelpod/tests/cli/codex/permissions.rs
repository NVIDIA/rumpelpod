// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Startup defaults must reach new app-servers without overriding saved threads.

use std::fs;
use std::time::Duration;

use indoc::formatdoc;
use rumpelpod::CommandExt;
use serde_json::json;

use super::common::{setup_codex_test_repo, CodexSession};
use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::executor_supports_stop;

const NO_BYPASS: &str = "--no-dangerously-bypass-approvals-and-sandbox";
const APP_SERVER_PATTERN: &str = "^/opt/rumpelpod/bin/codex app-server ";

fn configure_read_only(home: &TestHome) {
    let path = home.path().join(".codex/config.toml");
    let config = fs::read_to_string(&path).expect("read Codex config");
    fs::write(
        path,
        formatdoc! {r#"
            approval_policy = "on-request"
            default_permissions = ":read-only"
            {config}
        "#},
    )
    .expect("write read-only Codex config");
}

#[test]
fn codex_permissions_default_bypass_overrides_config() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();
    configure_read_only(&home);

    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.dismiss_dialogs();
    let contents = session.screen().contents();
    assert!(contents.contains("YOLO mode"), "{contents}");
    session.send("Run `printf %x 3735928559`.");
    session.wait_for_with_timeout("deadbeef", Duration::from_secs(30));
}

#[test]
fn codex_permissions_cli_opt_out_preserves_config() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();
    configure_read_only(&home);

    let mut session = CodexSession::spawn_named_with_rumpel_args(
        &repo,
        &daemon,
        home.path(),
        "test",
        &[NO_BYPASS],
        &[],
    );
    session.dismiss_dialogs();
    session.send("/status");
    session.wait_for_with_timeout("Read Only (Ask for approval)", Duration::from_secs(30));
}

#[test]
fn codex_permissions_config_opt_out_preserves_config() {
    let (home, repo, executor, daemon) = setup_codex_test_repo();
    configure_read_only(&home);
    let mut config: serde_json::Value =
        serde_json::from_str(&executor.json).expect("parse executor config");
    config["codex"] = json!({"dangerouslyBypassApprovalsAndSandbox": false});
    fs::write(repo.path().join(".rumpelpod.json"), config.to_string())
        .expect("write disabled bypass config");

    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.dismiss_dialogs();
    session.send("/status");
    session.wait_for_with_timeout("Read Only (Ask for approval)", Duration::from_secs(30));
}

#[test]
fn codex_permissions_cli_opt_out_allows_permission_flags() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();

    let mut session = CodexSession::spawn_named_with_rumpel_args(
        &repo,
        &daemon,
        home.path(),
        "test",
        &[NO_BYPASS],
        &["--sandbox", "read-only", "--ask-for-approval", "on-request"],
    );
    session.dismiss_dialogs();
    session.send("/status");
    session.wait_for_with_timeout("Read Only (Ask for approval)", Duration::from_secs(30));
}

#[test]
fn codex_permissions_new_threads_keep_default_bypass() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();
    configure_read_only(&home);

    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.dismiss_dialogs();
    session.send("What is the capital of France? Reply with just the city name, nothing else.");
    session.wait_for_with_timeout("Paris", Duration::from_secs(30));
    session.send("/new");
    // Codex prints the prior thread's usage after configuring the new thread.
    session.wait_for_with_timeout("Token usage: total=", Duration::from_secs(30));
    session.send("/status");
    session.wait_for_with_timeout("Full Access", Duration::from_secs(30));
}

#[test]
fn codex_permissions_picker_survives_new_thread() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();

    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.dismiss_dialogs();
    session.send("What is the capital of France? Reply with just the city name, nothing else.");
    session.wait_for_with_timeout("Paris", Duration::from_secs(30));
    session.send("/permissions");
    session.wait_for_with_timeout("4. Read Only", Duration::from_secs(30));
    session.write_raw(b"4\r");
    session.wait_for_with_timeout("Permissions updated to Read Only", Duration::from_secs(30));
    session.send("/new");
    session.wait_for_with_timeout("Token usage: total=", Duration::from_secs(30));
    session.send("/status");
    session.wait_for_with_timeout("Read Only (Ask for approval)", Duration::from_secs(30));
}

#[test]
fn codex_permissions_cached_proxy_refreshes_after_app_server_restart() {
    let (home, repo, _executor, daemon) = setup_codex_test_repo();
    configure_read_only(&home);
    write_test_devcontainer(&repo, "RUN apk add --no-cache procps", "");

    let mut first = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    first.dismiss_dialogs();
    first.send("What is the capital of France? Reply with just the city name, nothing else.");
    first.wait_for_with_timeout("Paris", Duration::from_secs(30));
    let original = app_server_command(&repo, &daemon);
    assert!(original.contains("default_permissions=\":danger-full-access\""));
    first.send("/exit");
    first.wait_for_exit();

    // Changing the launch preference must preserve both the running server
    // and the saved thread's permissions.
    let mut second = CodexSession::spawn_named_with_rumpel_args(
        &repo,
        &daemon,
        home.path(),
        "test",
        &[NO_BYPASS],
        &[],
    );
    second.wait_for_with_timeout("Paris", Duration::from_secs(30));
    let contents = second.screen().contents();
    assert!(contents.contains("YOLO mode"), "{contents}");
    assert_eq!(app_server_command(&repo, &daemon), original);
    second.send("/exit");
    second.wait_for_exit();

    let pid = original.split_whitespace().next().expect("app-server PID");
    pod_command(&repo, &daemon)
        .args(["enter", "test", "--", "kill", "-KILL", pid])
        .success()
        .expect("kill only this test's app-server");

    // The daemon and its cached proxy survive this failure. The replacement
    // server must receive the current opt-out, while the saved thread stays YOLO.
    let mut third = CodexSession::spawn_named_with_rumpel_args(
        &repo,
        &daemon,
        home.path(),
        "test",
        &[NO_BYPASS],
        &[],
    );
    third.wait_for_with_timeout("Paris", Duration::from_secs(30));
    let contents = third.screen().contents();
    assert!(contents.contains("YOLO mode"), "{contents}");
    let replacement = app_server_command(&repo, &daemon);
    assert_ne!(replacement.split_whitespace().next(), Some(pid));
    assert!(!replacement.contains(" -c "), "{replacement}");

    third.send("/new");
    third.wait_for_with_timeout("Token usage: total=", Duration::from_secs(30));
    third.send("/status");
    third.wait_for_with_timeout("Read Only (Ask for approval)", Duration::from_secs(30));
}

#[test]
fn codex_permissions_survive_pod_restart() {
    if !executor_supports_stop() {
        return;
    }
    let (home, repo, _executor, daemon) = setup_codex_test_repo();
    configure_read_only(&home);
    write_test_devcontainer(&repo, "RUN apk add --no-cache procps", "");

    let mut first = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    first.dismiss_dialogs();
    first.send("What is the capital of France? Reply with just the city name, nothing else.");
    first.wait_for_with_timeout("Paris", Duration::from_secs(30));
    first.send("/exit");
    first.wait_for_exit();
    pod_command(&repo, &daemon)
        .args(["stop", "--wait", "test"])
        .success()
        .expect("stop the test pod");

    let mut resumed = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    resumed.wait_for_with_timeout("Paris", Duration::from_secs(30));
    let contents = resumed.screen().contents();
    assert!(contents.contains("YOLO mode"), "{contents}");
    let command = app_server_command(&repo, &daemon);
    assert!(command.contains("approval_policy=\"never\""), "{command}");
    assert!(
        command.contains("default_permissions=\":danger-full-access\""),
        "{command}"
    );
}

fn app_server_command(repo: &TestRepo, daemon: &TestDaemon) -> String {
    let output = pod_command(repo, daemon)
        .args(["enter", "test", "--", "pgrep", "-af", APP_SERVER_PATTERN])
        .success()
        .expect("find the test's app-server process");
    let command = String::from_utf8(output).expect("app-server command is UTF-8");
    assert_eq!(command.lines().count(), 1, "{command}");
    command
}
