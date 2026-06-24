// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Common utilities for grok integration tests.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::common::{write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

/// The model grok is configured to use.  The coding-agent default
/// (`grok-build`) is gated to certain teams, so tests pin an
/// always-available model.  It is also the `model` field grok sends in
/// the chat request body, so it feeds the cache key.
pub const GROK_TEST_MODEL: &str = "grok-4.20-0309-non-reasoning";

/// Write a devcontainer that routes grok's model-catalog fetches at the
/// pod server's LLM cache proxy and supplies a (fake) API key.
///
/// grok's model-list client honors `GROK_MODELS_BASE_URL` /
/// `GROK_MODELS_LIST_URL`, so routing them at the proxy keeps startup
/// off the public internet (the pod is network isolated in tests).  The
/// chat/inference client is redirected separately via the per-model
/// `base_url` in `~/.grok/config.toml` (see `write_controlled_home`).
/// The fake key is enough for grok to consider itself logged in.
/// `${containerEnv:RUMPELPOD_SERVER_PORT}` resolves to the ephemeral
/// port container-serve exports (test-mode only).
fn write_grok_test_devcontainer(repo: &TestRepo) {
    let extra_json = r#",
        "remoteEnv": {
            "XAI_API_KEY": "xai-fake-test-key-for-llm-cache-proxy-00000000",
            "GROK_MODELS_BASE_URL": "http://127.0.0.1:${containerEnv:RUMPELPOD_SERVER_PORT}/llm-cache-proxy/xai/v1",
            "GROK_MODELS_LIST_URL": "http://127.0.0.1:${containerEnv:RUMPELPOD_SERVER_PORT}/llm-cache-proxy/xai/v1/models"
        }"#;

    write_test_devcontainer(repo, "", extra_json);
}

/// Write `~/.grok/config.toml` into the test home.
///
/// Defines a model whose per-model `base_url` points grok's
/// chat/inference traffic at the pod server's LLM cache proxy -- the one
/// override grok honors for the chat client (the top-level
/// `xai_api_base_url` does not).  `rumpel grok` copies `~/.grok` into the
/// pod, and the agent-files handler substitutes `${containerEnv:...}` in
/// test mode, so the port is filled in container-side.  The dummy key is
/// replaced by the cache proxy before any forward to the real API.
fn write_controlled_home(home: &TestHome) {
    let grok_dir = home.path().join(".grok");
    std::fs::create_dir_all(&grok_dir).expect("create .grok dir");
    std::fs::write(
        grok_dir.join("config.toml"),
        format!(
            "[model.localcache]\n\
             model = \"{GROK_TEST_MODEL}\"\n\
             base_url = \"http://127.0.0.1:${{containerEnv:RUMPELPOD_SERVER_PORT}}/llm-cache-proxy/xai/v1\"\n\
             api_key = \"dummy-key-replaced-by-cache-proxy\"\n\
             \n\
             [models]\n\
             default = \"localcache\"\n"
        ),
    )
    .expect("write grok config.toml");
}

/// Set up everything needed before running a grok command.
///
/// Uses `start_with_local_grok` so the daemon detects the grok binary
/// on the local machine and bakes it into the prepared image.
pub fn setup_grok_test_repo() -> (TestHome, TestRepo, ExecutorResources, TestDaemon) {
    let repo = TestRepo::new();
    write_grok_test_devcontainer(&repo);

    let home = TestHome::new();
    write_controlled_home(&home);
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start_with_local_grok(&home);
    std::fs::write(repo.path().join(".rumpelpod.json"), &executor.json)
        .expect("write .rumpelpod.json");
    (home, repo, executor, daemon)
}

// Tall enough to hold long responses without scrolling off, normal width.
const PTY_ROWS: u16 = 500;
const PTY_COLS: u16 = 80;
const SCREEN_DUMP_INTERVAL: Duration = Duration::from_secs(5);

/// An interactive grok CLI session running inside a container PTY.
///
/// Uses a vt100 terminal emulator to parse raw escape sequences and
/// provide rendered screen contents rather than raw byte matching.
pub struct GrokSession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    raw_log: Vec<u8>,
}

impl GrokSession {
    /// Spawn `rumpel grok --create test` inside a PTY.
    ///
    /// No `--model` is passed: the model (and its proxy `base_url`) come
    /// from the `[models] default` entry in the copied config.toml.
    /// Passing `--model <upstream-id>` would select a model without the
    /// per-model `base_url` and bypass the cache proxy.
    pub fn spawn(repo: &TestRepo, daemon: &TestDaemon, home: &Path) -> Self {
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
        // Match the daemon's narrowed PATH so the client half of
        // `rumpel grok` sees the same binaries on the local machine.
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

        cmd.args(["grok", "--create", "test"]);

        let child = pair.slave.spawn_command(cmd).expect("spawn rumpel grok");

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

        GrokSession {
            child,
            rx,
            parser,
            writer,
            raw_log: Vec::new(),
        }
    }

    /// Block until `needle` appears in the rendered terminal screen.
    /// Panics if the PTY closes before the needle appears.
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

    fn dump_screen(&self, waiting_for: &str) {
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

    /// Type text into the prompt and press Enter, waiting for the TUI to
    /// echo it first so callers do not need arbitrary sleeps.
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

impl Drop for GrokSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
