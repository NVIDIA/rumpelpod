// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Repro for Codex processes starting outside their workspaces.

use std::fs;
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};

use indoc::indoc;

use super::common::{setup_controlled_home, CodexSession};
use crate::common::{
    pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo, TEST_REPO_PATH,
};
use crate::executor::ExecutorResources;

#[test]
fn codex_app_server_starts_in_workspace() {
    println!("xtest:timeout=180");

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");

    let home = TestHome::new();
    setup_controlled_home(&home);
    let executor = ExecutorResources::setup(&home);
    let original_dir = std::env::current_dir().expect("read current dir");
    std::env::set_current_dir("/").expect("set daemon cwd");
    let daemon = TestDaemon::start_with_local_llm_clis(&home);
    std::env::set_current_dir(original_dir).expect("restore current dir");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).expect("write .rumpelpod.json");

    let mut session = CodexSession::spawn(&repo, &daemon, home.path(), &[]);
    session.dismiss_dialogs();

    let container_cwd = wait_for_codex_app_server_cwd(&repo, &daemon, Duration::from_secs(60));

    assert!(
        container_cwd == TEST_REPO_PATH,
        "Codex app-server should start in the workspace\n\
         container app-server cwd: {} (expected {})",
        container_cwd,
        TEST_REPO_PATH
    );
}

fn wait_for_codex_app_server_cwd(
    repo: &TestRepo,
    daemon: &TestDaemon,
    timeout: Duration,
) -> String {
    let deadline = Instant::now() + timeout;
    let mut last_output: Option<Output> = None;
    let read_command = indoc! {r#"
        for cmdline in /proc/[0-9]*/cmdline; do
            text=$(tr '\0' ' ' < "$cmdline")
            case "$text" in
                *"codex app-server --listen"*)
                    pid_dir=${cmdline%/cmdline}
                    readlink "$pid_dir/cwd"
                    exit 0
                    ;;
            esac
        done
        exit 1
    "#};

    while Instant::now() < deadline {
        let output = pod_command(repo, daemon)
            .args(["enter", "--create", "test", "--", "sh", "-c", read_command])
            .output()
            .expect("read codex app-server cwd from pod");
        if output.status.success() {
            let captured = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !captured.is_empty() {
                return captured;
            }
        }
        last_output = Some(output);
        thread::sleep(Duration::from_millis(500));
    }

    let diagnostic = match last_output {
        Some(output) => {
            let status = output.status;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            format!("last status: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}")
        }
        None => "no attempts made".to_string(),
    };
    panic!("timed out waiting for codex app-server cwd\n{diagnostic}");
}
