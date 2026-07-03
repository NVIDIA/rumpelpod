// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Common utilities for codex integration tests.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::common::{TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

/// Write controlled codex credentials into the test home directory.
///
/// Uses a fake API key because the LLM cache proxy on the local machine
/// replaces auth headers before forwarding to the real API.
/// The real OPENAI_API_KEY only needs to be set on the local machine
/// when running with RUMPELPOD_TEST_LLM_OFFLINE=0 to populate the cache.
pub fn setup_controlled_home(home: &TestHome) {
    let codex_dir = home.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).expect("create .codex dir");
    std::fs::write(
        codex_dir.join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake-test-key-for-llm-cache-proxy-00000000"}"#,
    )
    .expect("write auth.json");
    // Route API calls through the LLM cache proxy running on the pod
    // server via a custom model provider.  `supports_websockets = false`
    // forces Codex onto the plain HTTPS transport: the cache proxy only
    // speaks HTTP, so the default WebSocket transport would fail and
    // retry-loop before falling back.  `${containerEnv:...}` is
    // substituted in-pod when the config is extracted (test-mode only;
    // see pod/server.rs::agent_files_put_impl).
    std::fs::write(
        codex_dir.join("config.toml"),
        indoc::indoc! {r#"
            model_provider = "cacheproxy"

            [model_providers.cacheproxy]
            name = "cacheproxy"
            base_url = "http://127.0.0.1:${containerEnv:RUMPELPOD_SERVER_PORT}/llm-cache-proxy/openai/v1"
            wire_api = "responses"
            requires_openai_auth = true
            supports_websockets = false
        "#},
    )
    .expect("write config.toml");
}

/// Set up everything needed before running a codex command.
///
/// Uses `start_with_local_llm_clis` so the daemon can detect the codex
/// binary on the local machine and install it via prepare-image.
pub fn setup_codex_test_repo() -> (TestHome, TestRepo, ExecutorResources, TestDaemon) {
    let repo = TestRepo::new();

    crate::common::write_test_devcontainer(&repo, "", "");

    let home = TestHome::new();
    setup_controlled_home(&home);
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start_with_local_llm_clis(&home);
    std::fs::write(repo.path().join(".rumpelpod.json"), &executor.json)
        .expect("write .rumpelpod.json");
    (home, repo, executor, daemon)
}

const PTY_ROWS: u16 = 500;
const PTY_COLS: u16 = 80;
const SCREEN_DUMP_INTERVAL: Duration = Duration::from_secs(5);

/// An interactive Codex session running via `rumpel codex`.
pub struct CodexSession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    raw_log: Vec<u8>,
}

impl CodexSession {
    /// Spawn `rumpel codex test` inside a PTY.
    pub fn spawn(repo: &TestRepo, daemon: &TestDaemon, home: &Path, codex_args: &[&str]) -> Self {
        Self::spawn_named(repo, daemon, home, "test", codex_args)
    }

    /// Spawn `rumpel codex <pod_name>` inside a PTY.
    pub fn spawn_named(
        repo: &TestRepo,
        daemon: &TestDaemon,
        home: &Path,
        pod_name: &str,
        codex_args: &[&str],
    ) -> Self {
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
        // `rumpel codex` sees the same set of binaries on the local machine.
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

        cmd.args(["codex", "--create", pod_name]);
        if !codex_args.is_empty() {
            cmd.arg("--");
            for arg in codex_args {
                cmd.arg(arg);
            }
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn rumpel codex");

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

        CodexSession {
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
                    // Reap the child so the panic message can name the
                    // exit status: a fast PTY close almost always means
                    // `rumpel codex` itself exited early, and knowing
                    // whether it was code 0 vs. a failure cuts straight
                    // to whether rumpel or the TUI is at fault.
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

    /// Restart tests use this so connection failures surface before
    /// xtest kills the whole test process.
    pub fn wait_for_with_timeout(&mut self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut last_dump = Instant::now();

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining == Duration::ZERO {
                let contents = self.parser.screen().contents();
                let raw = String::from_utf8_lossy(&self.raw_log);
                panic!(
                    "timed out waiting for {:?}.\n\
                     Screen contents:\n{}\n\
                     Raw PTY bytes:\n{}",
                    needle, contents, raw
                );
            }

            let wait = remaining.min(SCREEN_DUMP_INTERVAL);
            match self.rx.recv_timeout(wait) {
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

    /// Dismiss startup dialogs by pressing Enter until we reach the
    /// main input prompt (not on the alternate screen).
    pub fn dismiss_dialogs(&mut self) {
        loop {
            self.wait_for("\u{203a}");

            while let Ok(bytes) = self.rx.try_recv() {
                self.raw_log.extend_from_slice(&bytes);
                self.parser.process(&bytes);
            }

            if !self.parser.screen().alternate_screen() {
                break;
            }

            self.writer.write_all(b"\r").expect("write Enter");
            self.writer.flush().expect("flush");
        }
    }

    /// Lifecycle tests need a bounded startup wait because stale
    /// remote state can close the TUI before it reaches a prompt.
    pub fn dismiss_dialogs_with_timeout(&mut self, timeout: Duration) {
        loop {
            self.wait_for_with_timeout("\u{203a}", timeout);

            while let Ok(bytes) = self.rx.try_recv() {
                self.raw_log.extend_from_slice(&bytes);
                self.parser.process(&bytes);
            }

            if !self.parser.screen().alternate_screen() {
                break;
            }

            self.writer.write_all(b"\r").expect("write Enter");
            self.writer.flush().expect("flush");
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

    /// Access the vt100 screen for inspecting terminal state.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Write raw bytes to the PTY without waiting for echo or pressing Enter.
    pub fn write_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write raw to PTY");
        self.writer.flush().expect("flush PTY writer");
    }

    /// Wait for the child process to exit.  Returns once the PTY reader
    /// disconnects (child exited) and the process has been reaped.
    pub fn wait_for_exit(&mut self) {
        loop {
            match self.rx.recv_timeout(Duration::from_secs(10)) {
                Ok(bytes) => {
                    self.raw_log.extend_from_slice(&bytes);
                    self.parser.process(&bytes);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for child to exit");
                }
            }
        }
        if let Err(e) = self.child.wait() {
            eprintln!("warning: failed to wait for child: {e}");
        }
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

    /// Lifecycle tests need bounded echo waits when stale remote state
    /// prevents the TUI from accepting input.
    pub fn send_with_timeout(&mut self, text: &str, timeout: Duration) {
        self.writer
            .write_all(text.as_bytes())
            .expect("write to PTY");
        self.writer.flush().expect("flush PTY writer");

        let needle_len = text.len().min(40);
        let needle = &text[text.len() - needle_len..];
        self.wait_for_with_timeout(needle, timeout);

        self.writer.write_all(b"\r").expect("write Enter to PTY");
        self.writer.flush().expect("flush PTY writer");
    }
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
