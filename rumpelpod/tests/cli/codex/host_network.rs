// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression coverage for Codex app-server ports with Docker host networking.

use std::fs;
use std::time::Duration;

use rumpelpod::CommandExt;

use super::common::{setup_controlled_home, CodexSession};
use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

const CODEX_APP_SERVER_PORT_FILE: &str = "/opt/rumpelpod/codex-app-server-port";
const CTRL_A: u8 = 0x01;

/// `--network=host` has no equivalent on a remote cluster node and the
/// daemon silently drops it there, so this test only makes sense
/// against the local Docker executor.
fn skip_unless_docker() -> bool {
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker
    ) {
        crate::executor::skip_test();
        return true;
    }
    false
}

#[test]
fn codex_app_server_ports_are_isolated_on_host_network() {
    println!("xtest:timeout=240");

    if skip_unless_docker() {
        return;
    }

    let repo = TestRepo::new();
    write_test_devcontainer(
        &repo,
        "",
        r#",
            "runArgs": ["--network=host"]"#,
    );

    let home = TestHome::new();
    setup_controlled_home(&home);
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start_with_local_llm_clis(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).expect("write .rumpelpod.json");

    let alpha_port = start_codex_and_read_port(&repo, &daemon, home.path(), "alpha");
    let beta_port = start_codex_and_read_port(&repo, &daemon, home.path(), "beta");

    assert_ne!(
        alpha_port, beta_port,
        "two host-networked pods must not share the Codex app-server port"
    );
}

fn start_codex_and_read_port(
    repo: &TestRepo,
    daemon: &TestDaemon,
    home: &std::path::Path,
    pod_name: &str,
) -> u16 {
    let mut session = CodexSession::spawn_named(repo, daemon, home, pod_name, &[]);
    session.dismiss_dialogs_with_timeout(Duration::from_secs(120));

    let output = pod_command(repo, daemon)
        .arg("enter")
        .arg(pod_name)
        .arg("--")
        .arg("cat")
        .arg(CODEX_APP_SERVER_PORT_FILE)
        .success()
        .unwrap_or_else(|e| panic!("reading codex app-server port in {pod_name}: {e}"));

    session.write_raw(&[CTRL_A, b'd']);
    session.wait_for_exit();

    String::from_utf8_lossy(&output)
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("parsing codex app-server port in {pod_name}: {e}"))
}
