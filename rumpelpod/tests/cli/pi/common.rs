// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Common utilities for pi integration tests.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use indoc::formatdoc;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::common::{write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

/// Pinned pi version for deterministic tests.
pub(super) const PI_VERSION: &str = "0.79.10";

/// Write a devcontainer that installs Node.js and the pinned pi CLI.
///
/// pi is a Node.js program, so the image needs a node runtime.  We
/// install it plus the pinned npm package directly in the Docker build
/// (rather than via the daemon's host-version detection) so the test
/// does not depend on a `pi` binary being present on the test host.
fn write_pi_test_devcontainer(repo: &TestRepo) {
    let extra_dockerfile = formatdoc! {r#"
        RUN apk add --no-cache nodejs npm
        RUN npm install -g --ignore-scripts {PI_NPM_PACKAGE}@{PI_VERSION}
    "#, PI_NPM_PACKAGE = "@earendil-works/pi-coding-agent"};

    // pi reads the Anthropic key from $ANTHROPIC_API_KEY (referenced by
    // models.json below).  The value is irrelevant: the LLM cache proxy
    // replaces the auth header before forwarding to the real API.
    // PI_OFFLINE suppresses pi's startup update/telemetry network calls.
    let extra_json = r#",
        "remoteEnv": {
            "ANTHROPIC_API_KEY": "sk-ant-fake-test-key-for-llm-cache-proxy-00000000",
            "PI_OFFLINE": "1"
        }"#;

    write_test_devcontainer(repo, &extra_dockerfile, extra_json);
}

/// Write the minimal pi config into the test home directory.
///
/// `models.json` overrides the Anthropic provider's base URL to point at
/// the pod server's LLM cache proxy route, so all of pi's
/// anthropic-messages traffic is cached the same way as the claude
/// tests.  `${containerEnv:RUMPELPOD_SERVER_PORT}` is substituted by the
/// pod server when the file is uploaded (test mode only).
///
/// `settings.json` pre-trusts the workspace; `ensure_pi_config` also
/// forces this, but writing it here keeps the controlled home
/// self-describing.
pub(super) fn setup_controlled_home(home: &TestHome) {
    let agent_dir = home.path().join(".pi/agent");
    std::fs::create_dir_all(&agent_dir).expect("create .pi/agent dir");

    std::fs::write(
        agent_dir.join("models.json"),
        r#"{
  "providers": {
    "anthropic": {
      "baseUrl": "http://127.0.0.1:${containerEnv:RUMPELPOD_SERVER_PORT}/llm-cache-proxy/anthropic",
      "api": "anthropic-messages",
      "apiKey": "$ANTHROPIC_API_KEY",
      "models": [
        {
          "id": "claude-haiku-4-5",
          "name": "Claude Haiku 4.5",
          "reasoning": false,
          "input": ["text"],
          "contextWindow": 200000,
          "maxTokens": 16384,
          "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }
        }
      ]
    }
  }
}
"#,
    )
    .expect("write models.json");

    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"defaultProjectTrust":"always"}"#,
    )
    .expect("write settings.json");
}

/// Set up the devcontainer, controlled home, and daemon -- everything
/// needed before running a `rumpel pi` command.
///
/// Drop order: `home` must outlive `executor` and `daemon`, so callers
/// must destructure in declaration order (Rust drops in reverse).
pub fn setup_pi_test_repo() -> (TestHome, TestRepo, ExecutorResources, TestDaemon) {
    let repo = TestRepo::new();
    write_pi_test_devcontainer(&repo);

    let home = TestHome::new();
    setup_controlled_home(&home);
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    std::fs::write(repo.path().join(".rumpelpod.json"), &executor.json)
        .expect("write .rumpelpod.json");
    (home, repo, executor, daemon)
}

const PTY_ROWS: u16 = 500;
const PTY_COLS: u16 = 80;
const SCREEN_DUMP_INTERVAL: Duration = Duration::from_secs(5);

/// An interactive pi session running inside a container PTY via `rumpel pi`.
pub struct PiSession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    raw_log: Vec<u8>,
}

impl PiSession {
    /// Spawn `rumpel pi --create test -- <pi_args>` inside a PTY.
    pub fn spawn(repo: &TestRepo, daemon: &TestDaemon, home: &Path, pi_args: &[&str]) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("create PTY");

        let mut cmd = CommandBuilder::new("rumpel");
        cmd.env_clear();
        cmd.cwd(repo.path());
        cmd.env("PATH", daemon.bin_dir.to_str().unwrap());
        cmd.env("HOME", home.to_str().unwrap());
        cmd.env(
            "RUMPELPOD_DAEMON_SOCKET",
            daemon.socket_path.to_str().unwrap(),
        );
        cmd.env(
            "RUMPELPOD_TEST_LLM_OFFLINE",
            std::env::var("RUMPELPOD_TEST_LLM_OFFLINE").unwrap_or_else(|_| "1".to_string()),
        );

        cmd.args(["pi", "--create", "test"]);
        if !pi_args.is_empty() {
            cmd.arg("--");
            for arg in pi_args {
                cmd.arg(arg);
            }
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn rumpel pi");

        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                if tx.send(buffer[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        let parser = vt100::Parser::new(PTY_ROWS, PTY_COLS, 0);

        PiSession {
            child,
            rx,
            parser,
            writer,
            raw_log: Vec::new(),
        }
    }

    /// Block until `needle` appears in the rendered terminal screen.
    pub fn wait_for(&mut self, needle: &str) -> String {
        let mut last_dump = Instant::now();

        loop {
            match self.rx.recv_timeout(SCREEN_DUMP_INTERVAL) {
                Ok(bytes) => {
                    self.raw_log.extend_from_slice(&bytes);
                    self.parser.process(&bytes);
                    while let Ok(more) = self.rx.try_recv() {
                        self.raw_log.extend_from_slice(&more);
                        self.parser.process(&more);
                    }
                    let contents = self.parser.screen().contents();
                    if contents.contains(needle) {
                        return contents;
                    }
                    if last_dump.elapsed() >= SCREEN_DUMP_INTERVAL {
                        self.dump_screen(needle);
                        last_dump = Instant::now();
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.dump_screen(needle);
                    last_dump = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let contents = self.parser.screen().contents();
                    let raw = String::from_utf8_lossy(&self.raw_log);
                    let exit = match self.child.try_wait() {
                        Ok(Some(status)) => format!("{status}"),
                        Ok(None) => "still running".to_string(),
                        Err(e) => format!("try_wait error: {e}"),
                    };
                    panic!(
                        "PTY closed before {:?} appeared on screen.\n\
                         rumpel exit: {}\n\
                         Screen contents:\n{}\n\
                         Raw PTY bytes:\n{}",
                        needle, exit, contents, raw
                    );
                }
            }
        }
    }

    pub fn wait_until_ready(&mut self) {
        self.wait_for("Pi can explain its own features");
    }

    pub fn dump_screen(&self, waiting_for: &str) {
        let screen = self.parser.screen();
        let contents = screen.contents();
        let trimmed = contents.trim();
        let alt = screen.alternate_screen();
        let (cur_row, cur_col) = screen.cursor_position();
        eprintln!(
            "[wait_for {:?}] alt_screen={} cursor=({},{}) screen:\n{}",
            waiting_for, alt, cur_row, cur_col, trimmed
        );
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Write raw bytes to the PTY without waiting for echo or Enter.
    pub fn write_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write raw to PTY");
        self.writer.flush().expect("flush PTY writer");
    }

    /// Type text into the prompt and press Enter.
    pub fn send(&mut self, text: &str) {
        self.writer
            .write_all(text.as_bytes())
            .expect("write to PTY");
        self.writer.flush().expect("flush PTY writer");

        let needle_len = text.len().min(40);
        let needle = &text[text.len() - needle_len..];
        self.wait_for(needle);

        self.writer.write_all(b"\r").expect("write Enter to PTY");
        self.writer.flush().expect("flush PTY writer");
    }
}

impl Drop for PiSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
