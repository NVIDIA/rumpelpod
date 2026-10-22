// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use futures_util::StreamExt;
use indoc::{formatdoc, indoc};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use rumpelpod::daemon::protocol::{ClientContext, ConnectPodRequest, Daemon, DaemonClient};
use rumpelpod::CommandExt;
use tokio::net::UnixListener;
use tokio::runtime::Runtime;

use crate::common::{
    create_commit, pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo,
    TEST_REPO_PATH, TEST_USER,
};
use crate::executor::{executor_supports_stop, ExecutorResources};

fn wait_connected(repo: &TestRepo, daemon: &TestDaemon) {
    let client = DaemonClient::new_unix(&daemon.socket_path);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if client
            .prepared_pod_connection(ConnectPodRequest {
                pod_name: "test".into(),
                repo_path: repo.path().to_path_buf(),
                client_context: ClientContext::default(),
            })
            .unwrap()
            .is_some()
        {
            return;
        }
        assert!(Instant::now() < deadline, "pod did not connect");
        thread::sleep(Duration::from_millis(20));
    }
}

// Replace the forwarded pod response without needing an old release binary.
// All other requests still reach the daemon and its real prepared pod.
fn exec_response_proxy(daemon: &TestDaemon, status: StatusCode) -> (PathBuf, Runtime) {
    let socket = daemon
        .home_path
        .join(format!("exec-{}.sock", status.as_u16()));
    let runtime = Runtime::new().unwrap();
    runtime.block_on(async {
        let listener = UnixListener::bind(&socket).unwrap();
        let client = reqwest::Client::builder()
            .unix_socket(daemon.socket_path.as_path())
            .build()
            .unwrap();
        let app = Router::new()
            .route(
                "/pod/exec",
                post(move |request: Request| async move {
                    let mut body = request.into_body().into_data_stream();
                    body.next().await.unwrap().unwrap();
                    // Give an eager stdin reader time to consume the input
                    // before rejecting the unsupported endpoint.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    status
                }),
            )
            .fallback(move |request: Request| {
                let client = client.clone();
                async move {
                    let url = format!("http://localhost{}", request.uri());
                    let response = client
                        .request(request.method().clone(), url)
                        .headers(request.headers().clone())
                        .body(reqwest::Body::wrap_stream(
                            request.into_body().into_data_stream(),
                        ))
                        .send()
                        .await
                        .unwrap();
                    Response::builder()
                        .status(response.status())
                        .body(Body::from_stream(response.bytes_stream()))
                        .unwrap()
                }
            });
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
    });
    (socket, runtime)
}

#[test]
fn exec_fallback_preserves_input_arguments_environment_directory_and_status() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::create_dir(repo.path().join("nested dir")).unwrap();
    fs::write(repo.path().join("nested dir/keep"), "").unwrap();
    Command::new("git")
        .current_dir(repo.path())
        .args(["add", "nested dir/keep"])
        .success()
        .unwrap();
    create_commit(
        repo.path(),
        "Keep the fallback working directory in the pod",
    );
    write_test_devcontainer(
        &repo,
        "",
        r#", "remoteEnv": {"EXEC_REMOTE": "${localEnv:EXEC_LOCAL}"}"#,
    );
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .env("EXEC_LOCAL", "from-launch")
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);
    let (socket, _proxy) = exec_response_proxy(&daemon, StatusCode::NOT_FOUND);
    // A fallback must use the existing connection without preparing the pod.
    fs::write(
        repo.path().join(".devcontainer/devcontainer.json"),
        "invalid json",
    )
    .unwrap();
    let mut child = pod_command(&repo, &daemon)
        .env("RUMPELPOD_DAEMON_SOCKET", &socket)
        .env("EXEC_LOCAL", "must-not-leak")
        .current_dir(repo.path().join("nested dir"))
        .args([
            "exec", "test", "--", "sh", "-c",
            "cat; id -un; pwd; printf '%s\\n' \"$EXEC_REMOTE\" \"$1\"; printf 'failure\\n' >&2; exit 42",
            "sh", "$HOME; $(touch /tmp/exec-fallback-expanded) 'quoted'",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input: Vec<u8> = (0..128 * 1024).map(|index| index as u8).collect();
    let mut expected = input.clone();
    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(move || stdin.write_all(&input).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert_eq!(output.status.code(), Some(42), "{:?}", output.stderr);
    expected.extend_from_slice(format!("{TEST_USER}\n{TEST_REPO_PATH}/nested dir\nfrom-launch\n$HOME; $(touch /tmp/exec-fallback-expanded) 'quoted'\n").as_bytes());
    assert_eq!(output.stdout, expected);
    assert_eq!(output.stderr, b"failure\n");
    pod_command(&repo, &daemon)
        .args([
            "exec",
            "test",
            "--",
            "test",
            "!",
            "-e",
            "/tmp/exec-fallback-expanded",
        ])
        .success()
        .unwrap();

    let mut child = pod_command(&repo, &daemon)
        .env("RUMPELPOD_DAEMON_SOCKET", &socket)
        .args(["exec", "test", "--", "sh", "-c", "printf ready; cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut bytes = [0; 5];
    stdout.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ready");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"input").unwrap();
    stdout.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"input");
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn exec_fallback_respects_explicit_terminal_selection() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);
    let (socket, _proxy) = exec_response_proxy(&daemon, StatusCode::NOT_FOUND);
    for tty in [false, true] {
        let mut args = vec![
            "-c",
            "export RUMPELPOD_DAEMON_SOCKET=$1; shift; exec rumpel \"$@\"",
            "sh",
            socket.to_str().unwrap(),
            "exec",
        ];
        if tty {
            args.push("-t");
        }
        let check = if tty {
            "test -t 0 && test -t 1 && test -t 2 || exit 1; printf terminal-ready; exit 42"
        } else {
            "test ! -t 0 && test ! -t 1 && test ! -t 2 || exit 1; printf terminal-ready; exit 42"
        };
        args.extend(["test", "--", "sh", "-c", check]);
        let mut terminal = ExecTerminal::spawn_local(&repo, &daemon, "sh", &args);
        terminal.wait_for("terminal-ready");
        assert_eq!(terminal.wait().exit_code(), 42);
        terminal.assert_restored();
    }
}

#[test]
fn exec_does_not_fall_back_on_other_http_errors() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let daemon = TestDaemon::start(&home);
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::CONFLICT,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
    ] {
        let (socket, _proxy) = exec_response_proxy(&daemon, status);
        let output = pod_command(&repo, &daemon)
            .env("RUMPELPOD_DAEMON_SOCKET", &socket)
            .args(["exec", "test", "--", "true"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("exec failed ({status})")),
            "{stderr}"
        );
    }
}

#[test]
fn exec_preserves_argv_environment_user_directory_and_status() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::create_dir_all(repo.path().join("nested dir")).unwrap();
    fs::write(repo.path().join("nested dir/keep"), "").unwrap();
    Command::new("git")
        .current_dir(repo.path())
        .args(["add", "nested dir/keep"])
        .success()
        .unwrap();
    create_commit(
        repo.path(),
        "Keep the test working directory in the pod checkout",
    );
    write_test_devcontainer(
        &repo,
        &formatdoc! {r#"
            RUN echo 'export EXEC_PROBED=from-profile' >> /home/{TEST_USER}/.bashrc
        "#},
        r#", "userEnvProbe": "interactiveShell", "remoteEnv": {"EXEC_REMOTE": "${localEnv:EXEC_LOCAL}"}"#,
    );
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .env("EXEC_LOCAL", "from-launch")
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let output = pod_command(&repo, &daemon)
        .current_dir(repo.path().join("nested dir"))
        .env("EXEC_LOCAL", "must-not-leak")
        .args([
            "exec", "test", "--", "sh", "-c",
            "id -un; pwd; printf '%s\\n' \"$EXEC_PROBED\" \"$EXEC_REMOTE\" \"$1\"; printf 'failure\\n' >&2; exit 42",
            "sh", "$HOME; $(touch /tmp/exec-argv-expanded)",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(42),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{TEST_USER}\n{TEST_REPO_PATH}/nested dir\nfrom-profile\nfrom-launch\n$HOME; $(touch /tmp/exec-argv-expanded)\n")
    );
    assert_eq!(output.stderr, b"failure\n");
    pod_command(&repo, &daemon)
        .args([
            "exec",
            "test",
            "--",
            "test",
            "!",
            "-e",
            "/tmp/exec-argv-expanded",
        ])
        .success()
        .unwrap();
    let output = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "echo shell syntax"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("starting exec program"));
}

#[test]
fn exec_streams_binary_input_and_output_before_stdin_eof() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut child = pod_command(&repo, &daemon)
        .args([
            "exec",
            "test",
            "--",
            "sh",
            "-c",
            "printf 'ready\\n'; tee /dev/stderr",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut greeting = [0; 6];
    stdout.read_exact(&mut greeting).unwrap();
    assert_eq!(
        &greeting, b"ready\n",
        "output must arrive while stdin is still open"
    );
    child.stdout = Some(stdout);
    let input: Vec<u8> = (0..3 * 1024 * 1024).map(|index| index as u8).collect();
    let expected = input.clone();
    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(move || stdin.write_all(&input).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, expected);
    assert_eq!(output.stderr, expected);
}

struct ExecTerminal {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Vec<u8>,
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    parser: vt100::Parser,
    original_termios: libc::termios,
}

impl ExecTerminal {
    fn spawn(repo: &TestRepo, daemon: &TestDaemon, args: &[&str]) -> Self {
        Self::spawn_local(repo, daemon, "rumpel", args)
    }

    fn spawn_local(repo: &TestRepo, daemon: &TestDaemon, program: &str, args: &[&str]) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 31,
                cols: 97,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let original_termios = Self::attributes(&*pair.master);
        let mut command = CommandBuilder::new(program);
        command.env_clear();
        command.env("HOME", &daemon.home_path);
        command.env("PATH", &daemon.bin_dir);
        command.env("RUMPELPOD_DAEMON_SOCKET", &daemon.socket_path);
        command.env("TERM", "xterm");
        command.cwd(repo.path());
        command.args(args);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || loop {
            let mut buffer = vec![0; 4096];
            let result = match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => {
                    buffer.truncate(count);
                    Ok(buffer)
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return,
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if tx.send(result).is_err() || failed {
                return;
            }
        });
        Self {
            master: pair.master,
            child,
            writer,
            output: Vec::new(),
            rx,
            parser: vt100::Parser::new(31, 97, 200),
            original_termios,
        }
    }

    fn wait_for(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !String::from_utf8_lossy(&self.output).contains(marker) {
            let bytes = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "waiting for {marker:?}: {error}; output: {:?}",
                        String::from_utf8_lossy(&self.output)
                    )
                })
                .unwrap();
            self.parser.process(&bytes);
            self.output.extend(bytes);
        }
    }

    fn wait_for_screen(&mut self, description: &str, predicate: impl Fn(&vt100::Screen) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !predicate(self.parser.screen()) {
            let bytes = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "waiting for {description}: {error}; screen: {:?}",
                        self.parser.screen().contents()
                    )
                })
                .unwrap();
            self.parser.process(&bytes);
            self.output.extend(bytes);
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }

    fn assert_restored(&self) {
        let restored = Self::attributes(&*self.master);
        assert_eq!(restored.c_iflag, self.original_termios.c_iflag);
        assert_eq!(restored.c_oflag, self.original_termios.c_oflag);
        assert_eq!(restored.c_cflag, self.original_termios.c_cflag);
        assert_eq!(restored.c_lflag, self.original_termios.c_lflag);
        assert_eq!(restored.c_cc, self.original_termios.c_cc);
        // SAFETY: both snapshots are initialized termios values.
        unsafe {
            assert_eq!(
                libc::cfgetispeed(&restored),
                libc::cfgetispeed(&self.original_termios)
            );
            assert_eq!(
                libc::cfgetospeed(&restored),
                libc::cfgetospeed(&self.original_termios)
            );
        }
    }

    fn attributes(master: &dyn MasterPty) -> libc::termios {
        // Some libc implementations leave unused termios slots untouched.
        // Initialize those slots so snapshot comparisons have stable values.
        let mut attributes = MaybeUninit::<libc::termios>::zeroed();
        // SAFETY: master owns the descriptor and attributes has enough space.
        let result =
            unsafe { libc::tcgetattr(master.as_raw_fd().unwrap(), attributes.as_mut_ptr()) };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        // SAFETY: the buffer was initialized and tcgetattr succeeded.
        unsafe { attributes.assume_init() }
    }

    fn wait(&mut self) -> portable_pty::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                loop {
                    match self
                        .rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(bytes) => {
                            let bytes = bytes.unwrap();
                            self.parser.process(&bytes);
                            self.output.extend(bytes);
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return status,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            panic!("exec terminal output remained open after client exit")
                        }
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "exec client did not exit: {:?}",
                String::from_utf8_lossy(&self.output)
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ExecTerminal {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = self.child.kill() {
                    eprintln!("killing exec test client: {error}");
                }
                if let Err(error) = self.child.wait() {
                    eprintln!("reaping exec test client: {error}");
                }
            }
            Err(error) => eprintln!("checking exec test client: {error}"),
        }
    }
}

#[test]
fn exec_tty_supports_password_prompts_resize_and_control_keys() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut terminal = ExecTerminal::spawn(
        &repo,
        &daemon,
        &[
            "exec",
            "--tty",
            "test",
            "--",
            "bash",
            "-c",
            indoc! {r#"
                set -eu
                test -t 0 && test -t 1 && test -t 2
                exec 3<>/dev/tty
                stty -echo <&3
                printf 'Password: ' >&3
                IFS= read -r password <&3
                test "$password" = 'terminal-secret'
                printf '\naccepted\n'
                printf 'TERM=%s\n' "$TERM"
                stty size <&3
                stty -icanon min 1 time 0 <&3
                printf 'byte-ready\n'
                byte=$(dd bs=1 count=1 status=none <&3)
                test "$byte" = x
                stty icanon <&3
                trap 'stty size <&3; printf "resized\n"' WINCH
                trap 'printf "interrupted\n"; exit 37' INT
                printf 'signal-ready\n'
                while :; do sleep 0.05; done
            "#},
        ],
    );
    terminal.wait_for("Password: ");
    terminal.writer.write_all(b"terminal-secret\n").unwrap();
    terminal.wait_for("byte-ready");
    assert!(
        !String::from_utf8_lossy(&terminal.output).contains("terminal-secret"),
        "password echoed by the client or pod terminal"
    );
    assert!(String::from_utf8_lossy(&terminal.output).contains("TERM=xterm"));
    assert!(String::from_utf8_lossy(&terminal.output).contains("31 97"));
    // No newline: this reaches the pod only if the client uses raw input.
    terminal.writer.write_all(b"x").unwrap();
    terminal.wait_for("signal-ready");
    terminal.resize(45, 111);
    terminal.wait_for("resized");
    assert!(String::from_utf8_lossy(&terminal.output).contains("45 111"));
    terminal.writer.write_all(b"\x03").unwrap();
    terminal.wait_for("interrupted");
    assert_eq!(terminal.wait().exit_code(), 37);
    terminal.assert_restored();

    let mut terminal = ExecTerminal::spawn(
        &repo,
        &daemon,
        &[
            "exec",
            "-t",
            "test",
            "--",
            "sh",
            "-c",
            "stty -echo; printf 'eof-ready\\n'; cat; printf 'eof-complete\\n'",
        ],
    );
    terminal.wait_for("eof-ready");
    terminal.writer.write_all(b"terminal-input\n\x04").unwrap();
    terminal.wait_for("eof-complete");
    assert!(String::from_utf8_lossy(&terminal.output).contains("terminal-input"));
    assert!(terminal.wait().success());
}

#[test]
fn exec_tty_preserves_scrollback_and_wraps_at_the_local_terminal_width() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut terminal = ExecTerminal::spawn(
        &repo,
        &daemon,
        &[
            "exec",
            "-t",
            "test",
            "--",
            "bash",
            "-c",
            indoc! {r#"
                for ((line=1; line<=80; line++)); do
                    printf 'line-%04d\n' "$line"
                done
                printf '%100sEND\n' '' | tr ' ' x
            "#},
        ],
    );
    assert!(terminal.wait().success());
    let screen = terminal.parser.screen();
    assert!(screen.contents().contains("line-0080"));
    assert!(!screen.contents().contains("line-0001"));
    assert_eq!(screen.rows(0, 97).nth(28).unwrap(), "x".repeat(97));
    assert_eq!(screen.rows(0, 97).nth(29).unwrap(), "xxxEND");
    terminal.parser.screen_mut().set_scrollback(200);
    assert!(terminal
        .parser
        .screen()
        .contents()
        .starts_with("line-0001\n"));
    terminal.assert_restored();
}

#[test]
fn exec_tty_pager_scrolls_with_keys_and_mouse_and_redraws_after_resize() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "RUN apk add --no-cache less", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut terminal = ExecTerminal::spawn(
        &repo,
        &daemon,
        &[
            "exec",
            "-t",
            "test",
            "--",
            "bash",
            "-c",
            indoc! {r#"
                set -eu
                for ((line=1; line<=200; line++)); do
                    printf 'line-%04d\n' "$line"
                done > /tmp/exec-pager-lines
                printf 'before-pager\n'
                less --mouse -P pager-ready /tmp/exec-pager-lines
                printf 'after-pager\n'
            "#},
        ],
    );
    terminal.wait_for_screen("the first pager page", |screen| {
        screen.alternate_screen()
            && screen.contents().starts_with("line-0001\n")
            && screen.contents().contains("pager-ready")
    });
    assert!(!terminal.parser.screen().contents().contains("line-0040"));

    terminal.writer.write_all(b"\x1b[6~").unwrap();
    terminal.wait_for_screen("PageDown to advance the pager", |screen| {
        !screen.contents().contains("line-0001")
            && screen.contents().contains("line-0040")
            && screen.contents().contains("pager-ready")
    });
    terminal.writer.write_all(b"\x1b[5~").unwrap();
    terminal.wait_for_screen("PageUp to return to the first page", |screen| {
        screen.contents().starts_with("line-0001\n") && screen.contents().contains("pager-ready")
    });

    terminal.writer.write_all(b"\x1b[<65;10;10M").unwrap();
    terminal.wait_for_screen("the mouse wheel to scroll down", |screen| {
        !screen.contents().contains("line-0001") && screen.contents().contains("pager-ready")
    });
    terminal.writer.write_all(b"\x1b[<64;10;10M").unwrap();
    terminal.wait_for_screen("the mouse wheel to scroll back up", |screen| {
        screen.contents().starts_with("line-0001\n") && screen.contents().contains("pager-ready")
    });

    terminal.resize(45, 111);
    terminal.wait_for_screen("the pager to fill the taller terminal", |screen| {
        screen.contents().starts_with("line-0001\n")
            && screen.contents().contains("line-0040")
            && screen.rows(0, 111).nth(44).unwrap().contains("pager-ready")
    });
    terminal.writer.write_all(b"q").unwrap();
    assert!(terminal.wait().success());
    let screen = terminal.parser.screen();
    assert!(!screen.alternate_screen());
    assert!(screen.contents().contains("before-pager"));
    assert!(screen.contents().contains("after-pager"));
    assert!(!screen.contents().contains("line-0040"));
    assert_eq!(screen.mouse_protocol_mode(), vt100::MouseProtocolMode::None);
    terminal.assert_restored();
}

#[test]
fn exec_tty_uses_stdout_dimensions_and_resizes_after_stdin_eof() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    for redirect_stdin in ["exec rumpel \"$@\" </dev/null", "printf '' | rumpel \"$@\""] {
        let mut terminal = ExecTerminal::spawn_local(
            &repo,
            &daemon,
            "sh",
            &[
                "-c",
                redirect_stdin,
                "sh",
                "exec",
                "-t",
                "test",
                "--",
                "bash",
                "-c",
                indoc! {r#"
                    set -eu
                    trap 'printf "resized="; stty size; exit 0' WINCH
                    printf 'initial='
                    stty size
                    printf 'resize-ready\n'
                    while :; do sleep 0.05; done
                "#},
            ],
        );
        terminal.wait_for("resize-ready");
        assert!(
            String::from_utf8_lossy(&terminal.output).contains("initial=31 97"),
            "{redirect_stdin}: {:?}",
            String::from_utf8_lossy(&terminal.output)
        );
        terminal.resize(45, 111);
        terminal.wait_for("resized=45 111");
        assert!(terminal.wait().success());
        terminal.assert_restored();
    }
}

#[test]
fn exec_tty_resizes_while_large_piped_input_is_blocked() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut terminal = ExecTerminal::spawn_local(
        &repo,
        &daemon,
        "sh",
        &[
            "-c",
            indoc! {r#"
                (
                    while ! test -e "$HOME/send-exec-input"; do sleep 0.01; done
                    head -c 16777216 /dev/zero
                ) | rumpel "$@"
            "#},
            "sh",
            "exec",
            "-t",
            "test",
            "--",
            "bash",
            "-c",
            indoc! {r#"
                set -eu
                stty raw -echo
                trap 'printf "resized="; stty size; exit 0' WINCH
                printf 'blocked-input-ready\n'
                while :; do sleep 0.05; done
            "#},
        ],
    );
    terminal.wait_for("blocked-input-ready");
    fs::write(home.path().join("send-exec-input"), "").unwrap();
    // The input exceeds the pipe, PTY, and HTTP/2 stream windows. No pod
    // process reads it, so a resize sharing that data stream cannot advance.
    thread::sleep(Duration::from_millis(500));
    let resized = Instant::now();
    terminal.resize(45, 111);
    terminal.wait_for("resized=45 111");
    assert!(
        resized.elapsed() < Duration::from_secs(5),
        "terminal resize waited for blocked stdin"
    );
    let status = terminal.wait();
    assert!(
        status.success(),
        "{status:?}: {:?}",
        String::from_utf8_lossy(&terminal.output)
    );
    terminal.assert_restored();
}

#[test]
fn exec_tty_restores_terminal_after_the_running_connection_fails() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut terminal = ExecTerminal::spawn(
        &repo,
        &daemon,
        &[
            "exec",
            "-t",
            "test",
            "--",
            "sh",
            "-c",
            indoc! {r#"
                stty -echo
                printf 'connection-failure-ready\n'
                read -r proceed
                kill -KILL "$PPID"
            "#},
        ],
    );
    terminal.wait_for("connection-failure-ready");
    let raw = terminal.master.get_termios().unwrap();
    assert_eq!(raw.local_flags.bits() & (libc::ECHO | libc::ICANON), 0);
    terminal.writer.write_all(b"continue\n").unwrap();
    assert!(!terminal.wait().success());
    assert!(String::from_utf8_lossy(&terminal.output).contains("outcome is unknown"));
    terminal.assert_restored();
}

#[test]
fn exec_tty_restores_terminal_after_startup_failure() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let daemon = TestDaemon::start(&home);
    let mut terminal =
        ExecTerminal::spawn(&repo, &daemon, &["exec", "-t", "missing", "--", "true"]);
    terminal.wait_for("not found");
    assert!(!terminal.wait().success());
    terminal.assert_restored();
}

#[test]
fn exec_tty_merges_output_and_exits_while_stdin_is_open() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut child = pod_command(&repo, &daemon)
        .args([
            "exec",
            "-t",
            "test",
            "--",
            "sh",
            "-c",
            "test -t 0 && test -t 1 && test -t 2 || exit 1; printf 'out\\n'; printf 'err\\n' >&2; exit 42",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!(output.status.code(), Some(42));
    assert_eq!(output.stdout, b"out\r\nerr\r\n");
    assert_eq!(output.stderr, b"");
}

#[test]
fn exec_tty_forwards_external_signals_to_foreground_jobs() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut terminal = ExecTerminal::spawn(
        &repo,
        &daemon,
        &[
            "exec",
            "-t",
            "test",
            "--",
            "bash",
            "--norc",
            "-ic",
            "sh -c 'printf foreground-ready; exec sleep 60'",
        ],
    );
    terminal.wait_for("foreground-ready");
    let pid = Pid::from_raw(terminal.child.process_id().unwrap() as i32);
    kill(pid, Signal::SIGINT).unwrap();
    assert_eq!(terminal.wait().exit_code(), 130);
    terminal.assert_restored();
}

#[test]
fn exec_tty_disconnect_terminates_background_jobs() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut terminal = ExecTerminal::spawn(
        &repo,
        &daemon,
        &[
            "exec",
            "-t",
            "test",
            "--",
            "bash",
            "--norc",
            "-ic",
            "sleep 60 & printf '%s\\n' \"$!\" > /tmp/exec-tty-job; printf job-ready; wait",
        ],
    );
    terminal.wait_for("job-ready");
    let output = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "cat", "/tmp/exec-tty-job"])
        .success()
        .unwrap();
    let pid = String::from_utf8(output).unwrap().trim().to_string();
    kill(
        Pid::from_raw(terminal.child.process_id().unwrap() as i32),
        Signal::SIGKILL,
    )
    .unwrap();
    terminal.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = pod_command(&repo, &daemon)
            .args([
                "exec",
                "test",
                "--",
                "sh",
                "-c",
                indoc! {r#"
                    if ! kill -0 "$1" 2>/dev/null; then exit 0; fi
                    grep -q '^State:.*Z' "/proc/$1/status"
                "#},
                "sh",
                &pid,
            ])
            .output()
            .unwrap();
        if output.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "terminal background job survived client disconnect: {:?}",
            output.stderr
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exec_reuses_connected_transport_for_first_and_concurrent_commands() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let calls = home.path().join("engine-calls");
    fs::write(&calls, "").unwrap();
    for engine in ["docker", "podman", "kubectl"] {
        let wrapper = home.bin_dir().join(engine);
        if !wrapper.exists() {
            continue;
        }
        let real = fs::canonicalize(&wrapper).unwrap();
        fs::remove_file(&wrapper).unwrap();
        let real = real.to_str().unwrap().replace('\'', "'\\''");
        let calls = calls.to_str().unwrap().replace('\'', "'\\''");
        fs::write(
            &wrapper,
            formatdoc! {"
            #!/bin/sh
            printf '<%s>' \"$@\" >> '{calls}'
            printf '\\n' >> '{calls}'
            exec '{real}' \"$@\"
        "},
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);
    let before = fs::read_to_string(&calls).unwrap();
    let execs_before = before
        .lines()
        .filter(|line| line.contains("<exec>"))
        .count();
    // A cached execution must also work while local launch configuration is
    // being edited, without falling through to pod preparation.
    fs::write(
        repo.path().join(".devcontainer/devcontainer.json"),
        "invalid json",
    )
    .unwrap();
    let started = Instant::now();
    pod_command(&repo, &daemon)
        .args([
            "exec",
            "-t",
            "test",
            "--",
            "sh",
            "-c",
            "test -t 0 && test -t 1 && test -t 2",
        ])
        .success()
        .unwrap();
    eprintln!("First connected terminal exec: {:?}", started.elapsed());

    let mut waiting = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = waiting.stdin.take().unwrap();
    stdin.write_all(b"ready").unwrap();
    let mut stdout = waiting.stdout.take().unwrap();
    let mut ready = [0; 5];
    stdout.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    waiting.stdout = Some(stdout);
    let commands: Vec<_> = (0..4)
        .map(|_| {
            pod_command(&repo, &daemon)
                .args(["exec", "test", "--", "true"])
                .spawn()
                .unwrap()
        })
        .collect();
    for mut command in commands {
        assert!(command.wait().unwrap().success());
    }
    drop(stdin);
    assert!(waiting.wait_with_output().unwrap().status.success());
    let after = fs::read_to_string(&calls).unwrap();
    assert_eq!(
        after.lines().filter(|line| line.contains("<exec>")).count(),
        execs_before,
        "exec opened a new container transport:\n{after}"
    );
}

#[test]
fn exec_blocked_input_does_not_stall_another_command() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut blocked_commands = Vec::new();
    for _ in 0..2 {
        let mut blocked = pod_command(&repo, &daemon)
            .args([
                "exec",
                "test",
                "--",
                "sh",
                "-c",
                "printf ready; exec sleep 60",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0; 5];
        blocked
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        assert_eq!(&ready, b"ready");
        let mut stdin = blocked.stdin.take().unwrap();
        let writer = thread::spawn(
            move || match stdin.write_all(&vec![b'x'; 16 * 1024 * 1024]) {
                Ok(()) => panic!("sleep unexpectedly consumed stdin"),
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe),
            },
        );
        blocked_commands.push((blocked, writer));
        thread::sleep(Duration::from_millis(200));
    }
    let mut probe = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "true"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = probe.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            probe.kill().unwrap();
            probe.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    for (mut blocked, writer) in blocked_commands {
        kill(Pid::from_raw(blocked.id() as i32), Signal::SIGINT).unwrap();
        assert_eq!(blocked.wait().unwrap().code(), Some(130));
        writer.join().unwrap();
    }
    assert!(
        status.is_some_and(|status| status.success()),
        "commands that stopped consuming stdin blocked another exec"
    );
}

#[test]
fn exec_blocked_output_does_not_stall_another_command() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut blocked_commands = Vec::new();
    for _ in 0..3 {
        let mut child = pod_command(&repo, &daemon)
            .args(["exec", "test", "--", "sh", "-c", "printf ready; exec yes"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0; 5];
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        assert_eq!(&ready, b"ready");
        blocked_commands.push(child);
        thread::sleep(Duration::from_millis(200));
    }
    let mut probe = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "true"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = probe.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            probe.kill().unwrap();
            probe.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    for mut child in blocked_commands {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert!(
        status.is_some_and(|status| status.success()),
        "commands with paused output consumers blocked another exec"
    );
}

#[test]
fn exec_forwards_signals_to_blocked_input_and_remaining_children() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut child = pod_command(&repo, &daemon)
        .args([
            "exec",
            "test",
            "--",
            "sh",
            "-c",
            "printf ready; exec sleep 60",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0; 5];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(&ready, b"ready");
    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(
        move || match stdin.write_all(&vec![b'x'; 16 * 1024 * 1024]) {
            Ok(()) => panic!("sleep unexpectedly consumed stdin"),
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe),
        },
    );
    // Fill the child pipe and HTTP flow-control window before interrupting.
    thread::sleep(Duration::from_millis(100));
    let interrupted = Instant::now();
    kill(Pid::from_raw(child.id() as i32), Signal::SIGINT).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(130));
    assert!(
        interrupted.elapsed() < Duration::from_secs(5),
        "interrupt was blocked by stdin"
    );
    writer.join().unwrap();

    // The shell exits immediately, but its background child retains stdout.
    // Cancellation must still reach that child while output is being drained.
    let mut child = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "sh", "-c", "sleep 60 & printf ready"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(&ready, b"ready");
    thread::sleep(Duration::from_millis(100));
    let interrupted = Instant::now();
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(
        interrupted.elapsed() < Duration::from_secs(5),
        "background child ignored termination"
    );
}

#[test]
fn exec_client_disconnect_terminates_children_without_losing_connection() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let mut child = pod_command(&repo, &daemon)
        .args([
            "exec",
            "test",
            "--",
            "sh",
            "-c",
            "sleep 60 & printf '%s\\n' \"$!\"; wait",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut pid = Vec::new();
    loop {
        let mut byte = [0];
        stdout.read_exact(&mut byte).unwrap();
        if byte[0] == b'\n' {
            break;
        }
        pid.push(byte[0]);
    }
    let pid = String::from_utf8(pid).unwrap();
    kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL).unwrap();
    child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = pod_command(&repo, &daemon)
            .args([
                "exec",
                "test",
                "--",
                "sh",
                "-c",
                indoc! {r#"
                if ! kill -0 "$1" 2>/dev/null; then exit 0; fi
                grep -q '^State:.*Z' "/proc/$1/status"
            "#},
                "sh",
                &pid,
            ])
            .output()
            .unwrap();
        if output.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "remote child survived client disconnect: {:?}",
            output.stderr
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exec_does_not_replay_a_command_when_the_server_dies() {
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);

    let output = pod_command(&repo, &daemon)
        .args([
            "exec",
            "test",
            "--",
            "sh",
            "-c",
            indoc! {r#"
            echo attempt >> /tmp/exec-attempts
            kill -KILL "$PPID"
        "#},
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("outcome is unknown"),
        "{:?}",
        output.stderr
    );

    pod_command(&repo, &daemon)
        .args(["connect", "test"])
        .success()
        .unwrap();
    wait_connected(&repo, &daemon);
    let output = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "cat", "/tmp/exec-attempts"])
        .success()
        .unwrap();
    assert_eq!(output, b"attempt\n");
}

#[test]
fn exec_requires_an_existing_connected_pod() {
    if !executor_supports_stop() {
        return;
    }
    let repo = TestRepo::new();
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    write_test_devcontainer(&repo, "", "");
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();
    let missing = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "true"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));
    pod_command(&repo, &daemon)
        .args(["enter", "--create", "test", "--", "true"])
        .success()
        .unwrap();
    pod_command(&repo, &daemon)
        .args(["stop", "--wait", "test"])
        .success()
        .unwrap();
    let stopped = pod_command(&repo, &daemon)
        .args(["exec", "test", "--", "true"])
        .output()
        .unwrap();
    assert!(!stopped.status.success());
    assert!(String::from_utf8_lossy(&stopped.stderr).contains("not connected"));
}
