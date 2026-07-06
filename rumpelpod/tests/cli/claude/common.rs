// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Common utilities for claude integration tests.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use indoc::formatdoc;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::common::{write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;

/// Pinned Claude Code version for deterministic tests.
const CLAUDE_CODE_VERSION: &str = "2.1.85";

/// npm registry URL for the pinned Claude Code version.
fn claude_code_tarball_url() -> String {
    format!("https://registry.npmjs.org/@anthropic-ai/claude-code/-/claude-code-{CLAUDE_CODE_VERSION}.tgz")
}

/// Write a devcontainer with a pinned Claude CLI.
///
/// Downloads the pinned npm package inside the Docker build and
/// extracts it without npm -- only the node runtime is needed.
/// Omitting npm also prevents Claude CLI's background `npm view`
/// update check.  Date drift in the request body is handled at the
/// cache-proxy layer (see normalize_cache_fields), so no Date hook
/// is needed here.
fn write_claude_test_devcontainer(repo: &TestRepo) {
    let tarball_url = claude_code_tarball_url();

    let extra_dockerfile = formatdoc! {r#"
        RUN apk add --no-cache nodejs curl
        RUN mkdir -p /usr/local/bin /usr/local/lib \
            && curl -fsSL "{tarball_url}" \
            | tar xz -C /usr/local/lib \
            && mv /usr/local/lib/package /usr/local/lib/claude-code \
            && ln -s /usr/local/lib/claude-code/cli.js /usr/local/bin/claude \
            && chmod +x /usr/local/lib/claude-code/cli.js
    "#};

    // ANTHROPIC_BASE_URL points at the pod server's LLM cache proxy
    // route.  `${containerEnv:RUMPELPOD_SERVER_PORT}` resolves to the
    // ephemeral port container-serve exports (test-mode only).
    //
    // The DISABLE_* vars make Claude deterministic so the recorded
    // cache replays offline. NONESSENTIAL_TRAFFIC stops the Statsig
    // fetch that otherwise toggles runtime feature gates per a random
    // per-run anonymousId -- gates that add the "ttl" prompt-cache hint
    // and inject a random available-skills reminder into tool results.
    // DISABLE_THINKING drops extended-thinking blocks, whose signature
    // is a fresh token every generation.
    let extra_json = r#",
        "remoteEnv": {
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:${containerEnv:RUMPELPOD_SERVER_PORT}/llm-cache-proxy/anthropic",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "CLAUDE_CODE_DISABLE_THINKING": "1"
        }"#;

    write_test_devcontainer(repo, &extra_dockerfile, extra_json);
}

/// Write the minimal config files needed for Claude tests into the
/// test home directory.
///
/// Contains only the config files needed for `copy_claude_config` in
/// the daemon, so tests don't depend on the real user's Claude config.
/// Includes fake OAuth credentials so the CLI considers the user
/// "logged in" without needing the local machine's real tokens.
pub(super) fn setup_controlled_home(home: &TestHome) {
    // Minimal .claude.json: skip onboarding and accept bypass mode.
    // strip_claude_json in the daemon will add per-project trust entries.
    std::fs::write(
        home.path().join(".claude.json"),
        r#"{"hasCompletedOnboarding":true,"lastOnboardingVersion":"999.0.0","bypassPermissionsModeAccepted":true}"#,
    )
    .expect("write controlled .claude.json");

    let claude_dir = home.path().join(".claude");
    std::fs::create_dir(&claude_dir).expect("create controlled .claude dir");
    std::fs::write(claude_dir.join("settings.json"), "{}").expect("write controlled settings.json");

    // The Claude CLI refuses to make API calls unless it sees OAuth
    // credentials (it shows "Not logged in" otherwise).  Provide fake
    // tokens with a far-future expiry so the CLI skips token refresh.
    // The LLM cache proxy replaces the auth header before forwarding
    // to the real API, so these dummy values never reach Anthropic.
    std::fs::write(
        claude_dir.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-fake-test-token-AAAAAAAAAAAAAAAAAA-BBBBBBBBBBBBBBBBBBB-CCCCCCCCCCCCCCCCCCCCCCCC","refreshToken":"sk-ant-ort01-fake-test-token-AAAAAAAAAAAAAAAAAA-BBBBBBBBBBBBBBBBBBB-CCCCCCCCCCCCCCCCCCCCCCCC","expiresAt":4102444800000,"scopes":["user:inference"],"subscriptionType":"max","rateLimitTier":"default_claude_max_5x"}}"#,
    )
    .expect("write controlled .credentials.json");

    // On k8s, the daemon needs kubeconfig to talk to the cluster.
    // Copy it from the real HOME into the fake one.
    let real_kube = std::path::PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
        .join(".kube/config");
    if let Ok(kubeconfig) = std::fs::read(&real_kube) {
        let kube_dir = home.path().join(".kube");
        std::fs::create_dir_all(&kube_dir).expect("create .kube dir");
        std::fs::write(kube_dir.join("config"), kubeconfig).expect("write kubeconfig");
    }
}

/// Write the devcontainer, start a daemon -- everything needed
/// before running a claude command.
///
/// Returns the home, executor, daemon, and repo so nothing is dropped
/// (and cleaned up) before the test finishes.
///
/// Drop order: `home` must outlive `executor` and `daemon`, so callers
/// must destructure in the right order (Rust drops in reverse
/// declaration order).
pub fn setup_claude_test_repo() -> (TestHome, TestRepo, ExecutorResources, TestDaemon) {
    let repo = TestRepo::new();
    write_claude_test_devcontainer(&repo);

    // Set up the controlled home before starting the executor so
    // copy_claude_config (which runs in the daemon) reads our files.
    let home = TestHome::new();
    setup_controlled_home(&home);
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    std::fs::write(repo.path().join(".rumpelpod.json"), &executor.json)
        .expect("write .rumpelpod.json");
    (home, repo, executor, daemon)
}

// Tall enough to hold long responses without scrolling off, normal width.
const PTY_ROWS: u16 = 500;
const PTY_COLS: u16 = 80;

/// How often to dump the screen contents while waiting.
const SCREEN_DUMP_INTERVAL: Duration = Duration::from_secs(5);

/// An interactive Claude CLI session running inside a container PTY.
///
/// Uses a vt100 terminal emulator to parse raw escape sequences and
/// provide rendered screen contents rather than raw byte matching.
pub struct ClaudeSession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    /// Raw bytes received from the PTY, kept for diagnostics on failure.
    raw_log: Vec<u8>,
}

impl ClaudeSession {
    /// Spawn `rumpel claude test` inside a PTY.
    ///
    /// Omitting `--dangerously-skip-permissions-hook` causes rumpel to
    /// pass `--dangerously-skip-permissions` directly to the claude CLI,
    /// which also suppresses the workspace trust dialog.
    ///
    /// `claude_args` are appended after `--` and forwarded to the
    /// claude CLI inside the container.
    pub fn spawn(
        repo: &TestRepo,
        daemon: &TestDaemon,
        home: &Path,
        model: &str,
        claude_args: &[&str],
    ) -> Self {
        Self::spawn_for_pod(repo, daemon, home, "test", true, model, claude_args)
    }

    /// Like `spawn`, but lets the caller pick the pod name and decide
    /// whether to pass `--create`.  Used by the fork test, which has
    /// to spawn `rumpel claude` on a pod the daemon already knows about
    /// (so --create would be a no-op at best, an error at worst).
    pub fn spawn_for_pod(
        repo: &TestRepo,
        daemon: &TestDaemon,
        home: &Path,
        pod_name: &str,
        create: bool,
        model: &str,
        claude_args: &[&str],
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

        // Note: we intentionally do NOT set the PTY to raw mode here.
        // Docker exec -it sets raw mode on its stdin (our PTY slave) when
        // it starts, so our settings would be overwritten anyway.

        let mut cmd = CommandBuilder::new("rumpel");
        cmd.env_clear();
        cmd.cwd(repo.path());
        // Match the daemon's narrowed PATH so the client half of
        // `rumpel claude` sees the same set of binaries on the local machine.
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

        cmd.arg("claude");
        if create {
            cmd.arg("--create");
        }
        cmd.args([pod_name, "--", "--model", model]);
        for arg in claude_args {
            cmd.arg(arg);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn rumpel claude");

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

        ClaudeSession {
            child,
            rx,
            parser,
            writer,
            raw_log: Vec::new(),
        }
    }

    /// Block until `needle` appears in the rendered terminal screen.
    ///
    /// Returns the full screen contents at the moment the needle was found.
    /// Panics if the PTY closes before the needle appears.  Does not
    /// enforce its own timeout -- the caller or test harness is
    /// responsible for aborting hung tests.
    ///
    /// Periodically dumps the screen contents so failures are debuggable.
    pub fn wait_for(&mut self, needle: &str) -> String {
        let mut last_dump = Instant::now();

        loop {
            match self.rx.recv_timeout(SCREEN_DUMP_INTERVAL) {
                Ok(bytes) => {
                    self.raw_log.extend_from_slice(&bytes);
                    self.parser.process(&bytes);
                    // Drain any additional buffered chunks before checking,
                    // so we don't miss text split across sends.
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
                    panic!(
                        "PTY closed before {:?} appeared on screen.\n\
                         Screen contents:\n{}\n\
                         Raw PTY bytes:\n{}",
                        needle, contents, raw
                    );
                }
            }
        }
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

    /// Access the vt100 screen for inspecting terminal state after exit.
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
        // Drain the PTY reader until it disconnects.
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
        // Reap the child so the exit status is collected.
        if let Err(e) = self.child.wait() {
            eprintln!("warning: failed to wait for child: {e}");
        }
    }

    /// Type text into the prompt and press Enter.
    ///
    /// Waits for the TUI to echo the text on screen before submitting,
    /// so callers do not need arbitrary sleeps between send and wait_for.
    pub fn send(&mut self, text: &str) {
        self.writer
            .write_all(text.as_bytes())
            .expect("write to PTY");
        self.writer.flush().expect("flush PTY writer");

        // Wait until the TUI has rendered the typed text.  Use a short
        // suffix to handle terminal line-wrapping (which inserts newlines
        // into screen contents).
        let needle_len = text.len().min(40);
        let needle = &text[text.len() - needle_len..];
        self.wait_for(needle);

        self.writer.write_all(b"\r").expect("write Enter to PTY");
        self.writer.flush().expect("flush PTY writer");
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
