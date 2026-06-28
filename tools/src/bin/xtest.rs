// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Custom test runner for rumpelpod integration tests.
//!
//! Rust's built-in test runner runs all tests inside one process,
//! captures output until completion, and has no per-test timeout or
//! retry mechanism.  Our integration tests talk to Docker, SSH
//! servers, and (optionally) remote Kubernetes clusters, so hangs and
//! flaky failures are unavoidable.  When a test hangs under the
//! built-in runner, the whole suite blocks silently with no indication
//! of what went wrong.
//!
//! This runner builds cross-architecture rumpel binaries (so the flat
//! layout matches production), then invokes each test as a separate
//! process.  That gives us:
//!
//! - **Timeouts** -- each test process is killed (via process-group
//!   SIGKILL) if it exceeds a deadline.  Tests can lower or raise
//!   their own timeout with `println!("xtest:timeout=SECS")`.
//!
//! - **Retries** -- failed tests are re-run up to N times (default 1),
//!   subject to a global retry budget (RETRY_BUDGET) that prevents
//!   flaky-test workarounds from masking widespread breakage.
//!
//! - **Skip** -- tests can declare themselves not applicable for the
//!   current configuration with `println!("xtest:skip")` and then
//!   return.  Skipped tests are not retried and do not count as
//!   failures.
//!
//! - **Visibility** -- output is buffered per test and dumped on
//!   failure, timeout, or Ctrl-C.  A periodic status line lists tests
//!   that have been running longer than LONG_RUNNING_THRESHOLD, so
//!   slow-but-not-stuck tests are distinguishable from hangs.
//!
//! - **Container cleanup** -- leftover Docker containers are removed
//!   after every run, so the pipeline's pre-flight "no stale
//!   containers" check stays green even after interrupted runs.
//!
//! # Binary size
//!
//! The dev profile in Cargo.toml sets opt-level=z and debug=0 to keep
//! binaries small (~28 MB vs ~263 MB with full debug info).  Every test
//! copies this binary into a Docker image, so size directly affects test
//! speed.
//!
//! # Usage
//!
//! ```text
//! cargo xtest [flags] [filter] [-- [test-binary-flags]]
//!
//! Flags:
//!   --test-threads N   parallel test processes (env: XTEST_JOBS, default: ncpus)
//!   --timeout SECS     per-test kill deadline  (env: XTEST_TIMEOUT, default: 120)
//!   --retries N        retry count on failure  (env: XTEST_RETRIES, default: 1)
//!   --executor NAME    run against podman or a k8s cluster (podman|eks|hetzner|k3d)
//! ```

use std::collections::HashSet;
use std::io::{pipe, BufRead, BufReader, Read};
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::Deserialize;
use tools::cargo_cmd;
use tools::CommandExt;

/// Maximum number of retries across all tests in a single run.
/// Keeps flaky-test workarounds from masking systemic problems.
const RETRY_BUDGET: usize = 3;

/// (cargo target triple, binary name in flat layout)
const LINUX_TARGETS: &[(&str, &str)] = &[
    ("x86_64-unknown-linux-musl", "rumpel-linux-amd64"),
    ("aarch64-unknown-linux-musl", "rumpel-linux-arm64"),
];

/// Alternate executors supported by the integration test harness.
#[derive(ValueEnum, Clone)]
enum Executor {
    Podman,
    Eks,
    Hetzner,
    K3d,
}

#[derive(Parser)]
#[command(trailing_var_arg = true)]
struct Cli {
    /// Kubernetes executor to target.
    #[arg(long)]
    executor: Option<Executor>,

    /// RuntimeClass name to inject into the executor's rumpelpod config.
    /// Use "runc" (the default) for the stock runtime; other values must
    /// correspond to a RuntimeClass registered in the target cluster, e.g.
    /// `sysbox-runc`, `runsc` (gVisor), or `kata`.  Only meaningful with
    /// --executor; ignored otherwise.
    #[arg(long)]
    runtime: Option<String>,

    /// Maximum number of tests to run in parallel (default: ncpus, env: XTEST_JOBS).
    #[arg(long = "test-threads")]
    jobs: Option<usize>,

    /// Per-test timeout in seconds (default: 120, env: XTEST_TIMEOUT).
    #[arg(long)]
    timeout: Option<u64>,

    /// Number of retries for failed tests (default: 1, env: XTEST_RETRIES).
    #[arg(long)]
    retries: Option<usize>,

    /// File containing test names to skip (one per line).
    /// Used by `cargo pipeline --continue` to resume from a previous run.
    #[arg(long, hide = true)]
    skip_file: Option<PathBuf>,

    /// Arguments passed through to cargo test.
    #[arg(allow_hyphen_values = true)]
    cargo_args: Vec<String>,
}

impl Executor {
    fn cloud_dir(&self, repo_root: &Path) -> PathBuf {
        match self {
            Executor::Podman => {
                panic!("podman executor does not have a cloud directory")
            }
            Executor::Eks => repo_root.join("cloud/eks"),
            Executor::Hetzner => repo_root.join("cloud/hetzner"),
            Executor::K3d => repo_root.join("cloud/k3d"),
        }
    }

    /// Collect env vars for running tests against this cluster.
    /// Returns a guard that keeps port-forward processes alive.
    fn apply(
        &self,
        env: &mut Vec<(String, String)>,
        runtime: Option<&str>,
    ) -> Result<ExecutorGuard> {
        if matches!(self, Executor::Podman) {
            if runtime.is_some() {
                anyhow::bail!("--runtime requires a Kubernetes executor");
            }
            env.push(("RUMPELPOD_TEST_EXECUTOR".into(), "podman".into()));
            eprintln!("Executor: podman");
            return Ok(ExecutorGuard::default());
        }

        let repo_root = tools::repo_root()?;
        let dir = self.cloud_dir(&repo_root);
        let dir = dir.canonicalize().with_context(|| {
            let path = dir.display();
            format!("cloud directory not found: {path}")
        })?;

        let config_path = dir.join("rumpelpod.json");
        let config_content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;

        let kubeconfig = dir.join("kubeconfig");
        if !kubeconfig.exists() {
            let path = kubeconfig.display();
            anyhow::bail!("kubeconfig not found: {path}");
        }

        // `--runtime <name>` is threaded to rumpelpod via env var
        // rather than the on-disk config format, so the rumpelpod
        // JSON/TOML schema stays untouched.  `"runc"` means "use the
        // containerd default", which is the same as not setting the
        // env var at all.
        if let Some(runtime) = runtime {
            if runtime != "runc" {
                env.push(("RUMPELPOD_RUNTIME_CLASS".into(), runtime.to_string()));
            }
        }

        env.push(("RUMPELPOD_EXECUTOR_CONFIG".into(), config_content));
        env.push(("KUBECONFIG".into(), kubeconfig.display().to_string()));

        let docker_config = dir.join("docker/config.json");
        if docker_config.exists() {
            env.push((
                "RUMPELPOD_TEST_DOCKER_CONFIG".into(),
                docker_config.display().to_string(),
            ));
        }

        let guard = match self {
            Executor::Podman => unreachable!("podman returned before k8s setup"),
            Executor::Eks => {
                self.apply_eks(env)?;
                ExecutorGuard::default()
            }
            Executor::Hetzner => self.apply_hetzner(&kubeconfig)?,
            // k3d uses a host-accessible registry and the default
            // buildx builder, so no port-forwards or buildx config
            // are needed here.
            Executor::K3d => ExecutorGuard::default(),
        };

        // Check for buildx config after apply_k3d, which creates it.
        let buildx_dir = dir.join("buildx");
        if buildx_dir.exists() {
            env.push((
                "RUMPELPOD_TEST_BUILDX_CONFIG".into(),
                buildx_dir.display().to_string(),
            ));
        }

        let dir_display = dir.display();
        eprintln!("Executor: {dir_display}");

        Ok(guard)
    }

    fn apply_eks(&self, env: &mut Vec<(String, String)>) -> Result<()> {
        // The ECR credential helper uses the AWS SDK which resolves
        // credentials from $HOME/.aws/.  Tests override HOME to an
        // isolated temp dir, so point the SDK at the real files.
        let home = std::env::var("HOME").context("HOME not set")?;
        let creds = PathBuf::from(format!("{home}/.aws/credentials"));
        if !creds.exists() {
            let path = creds.display();
            anyhow::bail!(
                "AWS credentials not found at {path}\n\
                 Run 'aws configure' to set up credentials for ECR access."
            );
        }
        env.push((
            "AWS_SHARED_CREDENTIALS_FILE".into(),
            creds.display().to_string(),
        ));
        let config = PathBuf::from(format!("{home}/.aws/config"));
        if config.exists() {
            env.push(("AWS_CONFIG_FILE".into(), config.display().to_string()));
        }

        // Tests run rumpel/daemon with PATH=home/.local/bin, which
        // contains only the binaries they explicitly opt in to.  EKS
        // needs the ECR credential helper on PATH during docker push,
        // so pass its directory via RUMPELPOD_EXTRA_PATH and let the
        // k8s executor setup symlink the binaries into the bin dir.
        let ecr_helper = tools::output(Command::new("which").arg("docker-credential-ecr-login"))
            .context("docker-credential-ecr-login not found on PATH")?;
        let ecr_helper_dir = Path::new(ecr_helper.trim())
            .parent()
            .context("docker-credential-ecr-login has no parent directory")?
            .display()
            .to_string();
        env.push(("RUMPELPOD_EXTRA_PATH".into(), ecr_helper_dir));

        Ok(())
    }

    fn apply_hetzner(&self, kubeconfig: &Path) -> Result<ExecutorGuard> {
        // Hetzner uses ghcr.io as registry, so only the buildkitd
        // port-forward is needed (no in-cluster registry).
        let mut guard = ExecutorGuard::default();
        let buildkit_child =
            start_port_forward(kubeconfig, "buildkit", "svc/buildkitd", 1234, 1234)?;
        guard.children.push(buildkit_child);
        eprintln!("Buildkitd port-forward: 127.0.0.1:1234 -> svc/buildkitd:1234");
        Ok(guard)
    }

    /// Set up k3d for ambient mode.
    ///
    /// Unlike --executor k3d (which sets RUMPELPOD_EXECUTOR_CONFIG and
    /// forces ALL tests into k8s mode), this just verifies that the k3d
    /// config exists.  Docker tests are unaffected.
    ///
    /// Returns None when the k3d directory or kubeconfig is missing
    /// (e.g. on macOS where no devcontainer provisions k3d).  K8s
    /// tests skip themselves via has_k8s_executor() in that case.
    fn apply_k3d_ambient(&self) -> Result<Option<ExecutorGuard>> {
        let repo_root = tools::repo_root()?;
        let dir = Executor::K3d.cloud_dir(&repo_root);
        let dir = match dir.canonicalize() {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let path = dir.display();
                eprintln!("k3d: {path} not found, k8s tests will skip");
                return Ok(None);
            }
            Err(e) => {
                let path = dir.display();
                return Err(e).with_context(|| format!("cloud/k3d directory: {path}"));
            }
        };

        let kubeconfig = dir.join("kubeconfig");
        if !kubeconfig.exists() {
            let path = kubeconfig.display();
            eprintln!("k3d: kubeconfig not found at {path}, k8s tests will skip");
            return Ok(None);
        }

        let dir_display = dir.display();
        eprintln!("k3d: {dir_display}");

        Ok(Some(ExecutorGuard::default()))
    }
}

/// Resources that must stay alive while cargo test runs.
///
/// Port-forward processes are killed on drop.
#[derive(Default)]
struct ExecutorGuard {
    children: Vec<std::process::Child>,
}

impl Drop for ExecutorGuard {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Start a kubectl port-forward on a fixed local port and wait for it
/// to accept connections.
fn start_port_forward(
    kubeconfig: &Path,
    namespace: &str,
    target: &str,
    local_port: u16,
    remote_port: u16,
) -> Result<std::process::Child> {
    let port_mapping = format!("{local_port}:{remote_port}");
    let kubeconfig_str = kubeconfig.display().to_string();

    let mut child = Command::new("kubectl")
        .env("KUBECONFIG", &kubeconfig_str)
        .args(["-n", namespace])
        .args(["port-forward", target, &port_mapping])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting kubectl port-forward to {target}"))?;

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if std::net::TcpStream::connect(format!("127.0.0.1:{local_port}")).is_ok() {
            break;
        }
        if let Ok(Some(exit)) = child.try_wait() {
            let mut stderr_str = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut stderr_str);
            }
            return Err(anyhow::anyhow!(
                "kubectl port-forward to {target}:{remote_port} exited with {exit}: {stderr_str}"
            ));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!(
                "kubectl port-forward to {target}:{remote_port} did not become ready within 15s"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok(child)
}

struct RunConfig {
    /// Test binaries discovered by `cargo test --no-run`.
    test_binaries: Vec<PathBuf>,
    /// Positional filter args from the user (e.g. "enter_").
    filter_args: Vec<String>,
    /// Test binary flags from the user (e.g. --ignored), after `--`.
    test_flags: Vec<String>,
    /// PATH with cross-arch binaries prepended.
    path_var: String,
    /// Env vars from executor setup.
    env_vars: Vec<(String, String)>,
    /// Per-test timeout.
    timeout: Duration,
    /// Number of retries for failed tests.
    retries: usize,
    /// Number of parallel jobs.
    jobs: usize,
}

struct TestResult {
    name: String,
    outcome: Outcome,
    duration: Duration,
    /// Shared buffer holding the captured output of the last attempt.
    /// The main thread can read partial output on interrupt.
    output: Arc<Mutex<String>>,
    /// Number of attempts (1 = passed on first try).
    attempts: usize,
    /// Log of retry decisions, printed before the final result.
    retry_log: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Passed,
    Failed,
    TimedOut,
    Skipped,
}

/// A test paired with the binary that contains it.
struct TestCase {
    binary: PathBuf,
    name: String,
}

fn load_skip_file(path: &Path) -> Result<HashSet<String>> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn env_var_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Split args at `--` into cargo-level args and test binary args.
fn split_at_dash(args: &[String]) -> (Vec<String>, Vec<String>) {
    match args.iter().position(|a| a == "--") {
        Some(pos) => (args[..pos].to_vec(), args[pos + 1..].to_vec()),
        None => (args.to_vec(), Vec::new()),
    }
}

/// Build test binaries and return their paths.
///
/// Uses `--message-format=json` to discover executable paths.
fn build_test_binaries(before_dash: &[String]) -> Result<Vec<PathBuf>> {
    let json_output = tools::output(
        cargo_cmd()
            .args(["test", "--no-run", "--message-format=json"])
            .args(before_dash),
    )?;

    let mut binaries = Vec::new();
    for line in json_output.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let is_artifact = msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-artifact");
        if !is_artifact {
            continue;
        }
        let is_test = msg
            .get("profile")
            .and_then(|p| p.get("test"))
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        if !is_test {
            continue;
        }
        if let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) {
            binaries.push(PathBuf::from(exe));
        }
    }
    Ok(binaries)
}

/// List tests from all test binaries, applying the user's filter.
fn list_tests(config: &RunConfig) -> Result<Vec<TestCase>> {
    let mut cases = Vec::new();
    for binary in &config.test_binaries {
        let mut cmd = Command::new(binary);
        cmd.args(&config.filter_args)
            .args(&config.test_flags)
            .arg("--list")
            .env("PATH", &config.path_var);
        for (k, v) in &config.env_vars {
            cmd.env(k, v);
        }
        let stdout = tools::output(&mut cmd)
            .with_context(|| format!("listing tests from {}", binary.display()))?;
        for line in stdout.lines() {
            if let Some(name) = line.strip_suffix(": test") {
                cases.push(TestCase {
                    binary: binary.clone(),
                    name: name.to_string(),
                });
            }
        }
    }
    Ok(cases)
}

struct SingleTestResult {
    outcome: Outcome,
    duration: Duration,
}

/// Run a single test with a timeout.
///
/// Tests can override the default timeout by printing directives to
/// stdout before doing any real work:
///
///     println!("xtest:timeout=300");
///     println!("xtest:skip");
///
/// Directives are stripped from the captured output.  The timeout
/// takes effect immediately (the reader thread updates an atomic that
/// the poll loop checks every 100 ms).
fn run_single_test(
    case: &TestCase,
    config: &RunConfig,
    output_buf: &Arc<Mutex<String>>,
    interrupted: &AtomicBool,
) -> SingleTestResult {
    let start = Instant::now();

    let (reader, writer) = match pipe() {
        Ok(rw) => rw,
        Err(e) => {
            *output_buf.lock().unwrap() = format!("failed to create pipe: {e}");
            return SingleTestResult {
                outcome: Outcome::Failed,
                duration: start.elapsed(),
            };
        }
    };
    let writer_clone = match writer.try_clone() {
        Ok(w) => w,
        Err(e) => {
            *output_buf.lock().unwrap() = format!("failed to clone pipe writer: {e}");
            return SingleTestResult {
                outcome: Outcome::Failed,
                duration: start.elapsed(),
            };
        }
    };

    let mut cmd = Command::new(&case.binary);
    cmd.args(["--exact", &case.name, "--nocapture", "--test-threads=1"])
        .args(&config.test_flags)
        .stdout(writer)
        .stderr(writer_clone)
        .env("PATH", &config.path_var);
    // Each test gets its own process group so we can kill the whole
    // tree on timeout.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    for (k, v) in &config.env_vars {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            *output_buf.lock().unwrap() = format!("failed to spawn test: {e}");
            return SingleTestResult {
                outcome: Outcome::Failed,
                duration: start.elapsed(),
            };
        }
    };

    // Close our copies of the write end so the pipe reaches EOF when
    // the child (and any children it spawned) exit.
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // Shared atomics so the reader thread can adjust the timeout
    // as soon as it sees an xtest: directive.
    let timeout_secs = Arc::new(AtomicU64::new(config.timeout.as_secs()));
    let skip_directive = Arc::new(AtomicBool::new(false));

    // Drain output in a background thread to avoid blocking the pipe
    // buffer.  Recognises xtest: directives and strips them from the
    // output.  Writes to the shared output_buf so the main thread can
    // dump partial output on interrupt.
    let reader_handle = {
        let timeout_secs = Arc::clone(&timeout_secs);
        let skip_directive = Arc::clone(&skip_directive);
        let output_buf = Arc::clone(output_buf);
        std::thread::spawn(move || {
            let mut accept_directives = true;
            let buf = BufReader::new(reader);
            for line in buf.lines() {
                match line {
                    Ok(line) => {
                        let mut out = output_buf.lock().unwrap();
                        if accept_directives {
                            if let Some((before, directive)) =
                                tools::split_xtest_directive_line(&line)
                            {
                                if parse_directive(directive, &timeout_secs, &skip_directive) {
                                    if !before.is_empty() {
                                        out.push_str(before);
                                        out.push('\n');
                                    }
                                    continue;
                                }
                            }

                            if !tools::is_xtest_prelude_line(&line) {
                                accept_directives = false;
                            }
                        }

                        out.push_str(&line);
                        out.push('\n');
                    }
                    Err(_) => break,
                }
            }
        })
    };

    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    if skip_directive.load(Relaxed) {
                        Outcome::Skipped
                    } else {
                        Outcome::Passed
                    }
                } else {
                    Outcome::Failed
                };
            }
            Ok(None) => {
                if interrupted.load(Relaxed) {
                    let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
                    let _ = child.wait();
                    break Outcome::Failed;
                }
                let deadline = start + Duration::from_secs(timeout_secs.load(Relaxed));
                if Instant::now() >= deadline {
                    let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
                    let _ = child.wait();
                    break Outcome::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                break Outcome::Failed;
            }
        }
    };

    let _ = reader_handle.join();
    SingleTestResult {
        outcome,
        duration: start.elapsed(),
    }
}

fn parse_directive(directive: &str, timeout_secs: &AtomicU64, skip: &AtomicBool) -> bool {
    if let Some(val) = directive.strip_prefix("timeout=") {
        if let Ok(secs) = val.parse::<u64>() {
            timeout_secs.store(secs, Relaxed);
            return true;
        }
    } else if directive.trim() == "skip" {
        skip.store(true, Relaxed);
        return true;
    }
    false
}

/// Run a test, retrying on failure.
///
/// Each retry consumes one unit from the shared `retry_budget`; when
/// the budget is exhausted no further retries happen for any test.
fn run_test_with_retries(
    case: &TestCase,
    config: &RunConfig,
    retry_budget: &AtomicU64,
    interrupted: &AtomicBool,
    output_buf: &Arc<Mutex<String>>,
) -> TestResult {
    let max_retries = config.retries;
    let mut retry_log = Vec::new();
    for attempt in 1.. {
        output_buf.lock().unwrap().clear();
        let result = run_single_test(case, config, output_buf, interrupted);
        if matches!(result.outcome, Outcome::Passed | Outcome::Skipped) || max_retries == 0 {
            return TestResult {
                name: case.name.clone(),
                outcome: result.outcome,
                duration: result.duration,
                output: Arc::clone(output_buf),
                attempts: attempt,
                retry_log,
            };
        }
        if attempt > max_retries {
            retry_log.push(format!(
                "attempt {attempt}: {} ({:.1}s), no retries left",
                outcome_label(result.outcome),
                result.duration.as_secs_f64(),
            ));
            return TestResult {
                name: case.name.clone(),
                outcome: result.outcome,
                duration: result.duration,
                output: Arc::clone(output_buf),
                attempts: attempt,
                retry_log,
            };
        }
        let budget_left = retry_budget.load(Relaxed) > 0;
        if !budget_left {
            retry_log.push(format!(
                "attempt {attempt}: {} ({:.1}s), retry budget exhausted",
                outcome_label(result.outcome),
                result.duration.as_secs_f64(),
            ));
            return TestResult {
                name: case.name.clone(),
                outcome: result.outcome,
                duration: result.duration,
                output: Arc::clone(output_buf),
                attempts: attempt,
                retry_log,
            };
        }
        retry_budget.fetch_sub(1, Relaxed);
        retry_log.push(format!(
            "attempt {attempt}: {} ({:.1}s), retrying",
            outcome_label(result.outcome),
            result.duration.as_secs_f64(),
        ));
    }
    unreachable!()
}

fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Passed => "ok",
        Outcome::Failed => "FAILED",
        Outcome::TimedOut => "TIMED OUT",
        Outcome::Skipped => "SKIPPED",
    }
}

fn retry_suffix(attempts: usize) -> String {
    match attempts {
        1 => String::new(),
        2 => ", 1 retry".to_string(),
        n => format!(", {} retries", n - 1),
    }
}

/// How long a test must be running before it shows up in the
/// periodic "still running" status line.
const LONG_RUNNING_THRESHOLD: Duration = Duration::from_secs(60);

/// How often the main thread checks for long-running tests when no
/// results arrive.
const STATUS_INTERVAL: Duration = Duration::from_secs(30);

enum WorkerEvent {
    Started {
        name: String,
        at: Instant,
        output: Arc<Mutex<String>>,
    },
    Finished(TestResult),
}

/// Run all tests in parallel, printing progress as they complete.
fn run_tests(cases: Vec<TestCase>, config: &RunConfig) -> Vec<TestResult> {
    let total = cases.len();
    let queue = Mutex::new(cases.into_iter());
    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let wall_start = Instant::now();
    let retry_budget = AtomicU64::new(RETRY_BUDGET as u64);
    let interrupted = Arc::new(AtomicBool::new(false));

    // On Ctrl-C, set the interrupted flag so workers stop picking up
    // new cases, kill their children, and the main thread can dump
    // in-flight output.
    {
        let interrupted = Arc::clone(&interrupted);
        let _ = ctrlc::set_handler(move || {
            interrupted.store(true, Relaxed);
        });
    }

    std::thread::scope(|s| {
        for _ in 0..config.jobs {
            let tx = tx.clone();
            let queue = &queue;
            let retry_budget = &retry_budget;
            let interrupted = &interrupted;
            s.spawn(move || loop {
                if interrupted.load(Relaxed) {
                    break;
                }
                let case = queue.lock().unwrap().next();
                let Some(case) = case else { break };
                let output_buf = Arc::new(Mutex::new(String::new()));
                let _ = tx.send(WorkerEvent::Started {
                    name: case.name.clone(),
                    at: Instant::now(),
                    output: Arc::clone(&output_buf),
                });
                let result =
                    run_test_with_retries(&case, config, retry_budget, interrupted, &output_buf);
                let _ = tx.send(WorkerEvent::Finished(result));
            });
        }
        drop(tx);

        let mut results = Vec::with_capacity(total);
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut timed_out = 0usize;
        let mut skipped = 0usize;
        let mut retried = 0usize;
        // Tests currently being worked on, including retries.
        let mut in_flight: Vec<(String, Instant, Arc<Mutex<String>>)> = Vec::new();

        loop {
            let event = match rx.recv_timeout(STATUS_INTERVAL) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            match event {
                Some(WorkerEvent::Started { name, at, output }) => {
                    in_flight.push((name, at, output));
                    continue;
                }
                Some(WorkerEvent::Finished(result)) => {
                    in_flight.retain(|(name, _, _)| *name != result.name);

                    let n = results.len() + 1;
                    let secs = result.duration.as_secs_f64();
                    let retries = retry_suffix(result.attempts);

                    let status = match result.outcome {
                        Outcome::Passed => {
                            passed += 1;
                            format!("ok{retries}")
                        }
                        Outcome::Failed => {
                            failed += 1;
                            format!("FAILED{retries}")
                        }
                        Outcome::TimedOut => {
                            timed_out += 1;
                            format!("TIMED OUT{retries}")
                        }
                        Outcome::Skipped => {
                            skipped += 1;
                            "SKIPPED".to_string()
                        }
                    };

                    if result.attempts > 1 {
                        retried += 1;
                    }

                    for entry in &result.retry_log {
                        eprintln!("        {} {entry}", result.name);
                    }
                    eprintln!("[{n:>3}/{total}] {} ... {status} ({secs:.1}s)", result.name);

                    if matches!(result.outcome, Outcome::Failed | Outcome::TimedOut) {
                        let output = result.output.lock().unwrap();
                        if !output.is_empty() {
                            for line in output.lines() {
                                eprintln!("  | {line}");
                            }
                        }
                    }

                    results.push(result);
                }
                None => {}
            }

            let now = Instant::now();
            let mut long: Vec<(&str, u64)> = in_flight
                .iter()
                .filter_map(|(name, started, _)| {
                    let elapsed = now.duration_since(*started);
                    if elapsed >= LONG_RUNNING_THRESHOLD {
                        Some((name.as_str(), elapsed.as_secs()))
                    } else {
                        None
                    }
                })
                .collect();
            if !long.is_empty() {
                long.sort_by_key(|entry| std::cmp::Reverse(entry.1));
                let items: Vec<String> = long
                    .iter()
                    .map(|(name, s)| format!("{name} ({s}s)"))
                    .collect();
                eprintln!("        ... waiting: {}", items.join(", "));
            }
        }

        // If interrupted, dump partial output from tests that were
        // still running.
        if interrupted.load(Relaxed) && !in_flight.is_empty() {
            eprintln!();
            eprintln!("Interrupted -- output from in-flight tests:");
            for (name, started, output_buf) in &in_flight {
                let elapsed = Instant::now().duration_since(*started).as_secs_f64();
                let output = output_buf.lock().unwrap();
                eprintln!();
                eprintln!("  {name} ({elapsed:.1}s):");
                if output.is_empty() {
                    eprintln!("  | (no output)");
                } else {
                    for line in output.lines() {
                        eprintln!("  | {line}");
                    }
                }
            }
        }

        let completed = results.len();
        let not_started = total - completed;
        let total_skipped = skipped + not_started;
        let wall_secs = wall_start.elapsed().as_secs_f64();
        eprintln!();
        let retry_note = if retried > 0 {
            format!(" ({retried} retried)")
        } else {
            String::new()
        };
        if total_skipped > 0 {
            eprintln!(
                "{completed}/{total} tests: {passed} passed, {failed} failed, \
                 {timed_out} timed out, {total_skipped} skipped{retry_note} ({wall_secs:.0}s)",
            );
        } else {
            eprintln!(
                "{total} tests: {passed} passed, {failed} failed, \
                 {timed_out} timed out{retry_note} ({wall_secs:.0}s)",
            );
        }

        results
    })
}

/// Remove leftover test containers so failed tests do not poison the next run.
fn cleanup_containers(executor: &Executor) {
    let engine = match executor {
        Executor::Podman => "podman",
        Executor::Eks | Executor::Hetzner | Executor::K3d => "docker",
    };
    let args: &[&str] = match executor {
        Executor::Podman => &[
            "ps",
            "-a",
            "--filter",
            "label=dev.rumpelpod.repo_path",
            "--format",
            "{{.Names}}",
        ],
        Executor::Eks | Executor::Hetzner | Executor::K3d => {
            &["ps", "-a", "--format", "{{.Names}}"]
        }
    };
    let output = match Command::new(engine).args(args).output() {
        Ok(out) if out.status.success() => out,
        _ => return,
    };
    let names = String::from_utf8_lossy(&output.stdout);
    let to_remove: Vec<&str> = names
        .lines()
        .filter(|name| {
            let name = name.trim();
            !name.is_empty() && !name.starts_with("k3d-") && !name.ends_with("-registry.localhost")
        })
        .collect();
    if to_remove.is_empty() {
        return;
    }
    eprintln!(
        "warning: {} leftover {engine} containers (per-test cleanup missed them)",
        to_remove.len()
    );
    let _ = Command::new(engine)
        .arg("rm")
        .arg("-f")
        .args(&to_remove)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Locate the kubeconfig + context for the given executor's cluster
/// directory.  Returns `None` when the executor has no cluster
/// configured locally (e.g. ambient k3d on a host without k3d
/// installed) so callers can no-op silently.
fn k8s_cluster_for_executor(executor: &Executor) -> Option<(PathBuf, String)> {
    if matches!(executor, Executor::Podman) {
        return None;
    }

    // Mirror only the fields we actually need.  Avoiding the full
    // `rumpelpod::config::JsonConfig` keeps the tools crate from
    // depending on the rumpelpod lib.
    #[derive(Deserialize)]
    struct Config {
        kubernetes: Option<Kubernetes>,
    }
    #[derive(Deserialize)]
    struct Kubernetes {
        context: String,
    }

    let repo_root = tools::repo_root().ok()?;
    let dir = executor.cloud_dir(&repo_root);
    let config_content = std::fs::read_to_string(dir.join("rumpelpod.json")).ok()?;
    let config: Config = json5::from_str(&config_content).ok()?;
    let kubernetes = config.kubernetes?;
    let kubeconfig = dir.join("kubeconfig");
    if !kubeconfig.exists() {
        return None;
    }
    Some((kubeconfig, kubernetes.context))
}

/// List the per-test sibling namespaces currently present on the
/// executor's cluster (those carrying the
/// `rumpelhub/test-namespace=true` label written by
/// `tests/cli/executor.rs::K8sTestNamespace`).  Namespaces in the
/// `Terminating` phase are skipped: they have a delete already in
/// flight, so they neither need manual intervention nor count as
/// leftovers a fresh run would clobber.
fn list_test_namespaces(kubeconfig: &Path, context: &str) -> Vec<String> {
    let output = match Command::new("kubectl")
        .args(["--kubeconfig", &kubeconfig.display().to_string()])
        .args(["--context", context])
        .args([
            "get",
            "namespaces",
            "-l",
            "rumpelhub/test-namespace=true",
            "-o",
            "jsonpath={range .items[*]}{.metadata.name} {.status.phase}\n{end}",
        ])
        .output()
    {
        Ok(out) if out.status.success() => out,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let name = parts.next()?;
            let phase = parts.next().unwrap_or("");
            if phase == "Terminating" {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Refuse to start the run if the executor's cluster already has
/// per-test namespaces from a prior run.  These have to be cleaned
/// up by hand: a stuck namespace usually means a previous run left
/// pods or finalizers behind that the operator should investigate
/// before launching another batch on top.
fn check_no_leftover_k8s_namespaces(executor: &Executor) -> Result<()> {
    let Some((kubeconfig, context)) = k8s_cluster_for_executor(executor) else {
        return Ok(());
    };
    let leftover = list_test_namespaces(&kubeconfig, &context);
    if leftover.is_empty() {
        return Ok(());
    }
    let mut msg = String::from(
        "refusing to run: the executor cluster already has leftover \
         per-test namespaces from a previous run.  Inspect them and \
         delete them by hand, e.g.\n\n  kubectl --context ",
    );
    msg.push_str(&context);
    msg.push_str(" delete namespace");
    for name in &leftover {
        msg.push(' ');
        msg.push_str(name);
    }
    msg.push_str("\n\nleftover namespaces:\n");
    for name in &leftover {
        msg.push_str("  - ");
        msg.push_str(name);
        msg.push('\n');
    }
    Err(anyhow::anyhow!(msg))
}

/// Delete every per-test sibling namespace on the executor's
/// cluster, regardless of age.  Each test's `Drop` already fires a
/// non-blocking delete; this end-of-run sweep catches anything that
/// slipped through (e.g. a panicking test or a SIGKILL'd process)
/// and short-circuits the next run's leftover check.
fn cleanup_k8s_namespaces(executor: &Executor) {
    let Some((kubeconfig, context)) = k8s_cluster_for_executor(executor) else {
        return;
    };
    let names = list_test_namespaces(&kubeconfig, &context);
    if names.is_empty() {
        return;
    }
    let _ = Command::new("kubectl")
        .args(["--kubeconfig", &kubeconfig.display().to_string()])
        .args(["--context", &context])
        .arg("delete")
        .arg("namespace")
        .arg("--wait=false")
        .args(&names)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    let default_jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let jobs = cli
        .jobs
        .unwrap_or_else(|| env_var_or("XTEST_JOBS", default_jobs));
    let timeout_secs = cli
        .timeout
        .unwrap_or_else(|| env_var_or("XTEST_TIMEOUT", 120));
    let retries = cli
        .retries
        .unwrap_or_else(|| env_var_or("XTEST_RETRIES", 1));

    let mut targets: Vec<(&str, &str)> = Vec::new();
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        targets.push(("aarch64-apple-darwin", "rumpel-darwin-arm64"));
    }
    targets.extend_from_slice(LINUX_TARGETS);

    for (triple, _) in &targets {
        eprint!("Building rumpel for {triple}... ");
        cargo_cmd()
            .args(["build", "--bin", "rumpel", "--target", triple])
            .success()
            .with_context(|| format!("building rumpel for {triple}"))?;
        eprintln!("ok");
    }

    let tmp = tempfile::tempdir().context("creating temp dir")?;

    for (triple, name) in &targets {
        let src = Path::new("target")
            .join(triple)
            .join("debug")
            .join("rumpel");
        let dst = tmp.path().join(name);
        std::fs::copy(&src, &dst).with_context(|| {
            let src = src.display();
            let dst = dst.display();
            format!("copying {src} -> {dst}")
        })?;
    }

    let native_name = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "rumpel-darwin-arm64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "rumpel-linux-amd64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "rumpel-linux-arm64"
    } else {
        return Err(anyhow::anyhow!(
            "unsupported host platform: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    };
    symlink(native_name, tmp.path().join("rumpel"))
        .context("symlinking rumpel to native binary")?;

    let path_var = std::env::var("PATH").context("PATH not set")?;
    let tmp_path = tmp.path().display();
    let path_var = format!("{tmp_path}:{path_var}");

    let mut env_vars = Vec::new();
    let _guard = if let Some(ref executor) = cli.executor {
        // Explicit --executor: redirect executor-agnostic tests to
        // the requested backend.
        Some(executor.apply(&mut env_vars, cli.runtime.as_deref())?)
    } else {
        if cli.runtime.is_some() {
            anyhow::bail!("--runtime requires --executor");
        }
        // The devcontainer provisions a k3d cluster during boot.
        // Set up port-forwards under K3D-specific env vars so k8s
        // tests pick them up without affecting docker tests (which
        // check RUMPELPOD_EXECUTOR_CONFIG via executor_mode()).
        Executor::K3d.apply_k3d_ambient()?
    };

    let cleanup_executor = cli.executor.clone().unwrap_or(Executor::K3d);
    check_no_leftover_k8s_namespaces(&cleanup_executor)?;

    let (before_dash, after_dash) = split_at_dash(&cli.cargo_args);

    let test_binaries = build_test_binaries(&before_dash)?;
    if test_binaries.is_empty() {
        eprintln!("No test binaries found");
        return Ok(ExitCode::SUCCESS);
    }

    // Separate the user's filter (positional args) from cargo flags so
    // the filter is passed to the test binary during listing but not to
    // individual test runs (which use --exact).
    let filter_args: Vec<String> = before_dash
        .iter()
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .collect();
    let test_flags: Vec<String> = after_dash
        .iter()
        .filter(|a| a.starts_with('-'))
        .cloned()
        .collect();

    let config = RunConfig {
        test_binaries,
        filter_args,
        test_flags,
        path_var,
        env_vars,
        timeout: Duration::from_secs(timeout_secs),
        retries,
        jobs,
    };

    let mut cases = list_tests(&config)?;

    if let Some(ref skip_file) = cli.skip_file {
        let skip_names = load_skip_file(skip_file)?;
        let before = cases.len();
        cases.retain(|c| !skip_names.contains(&c.name));
        let skipped = before - cases.len();
        if skipped > 0 {
            eprintln!("Skipping {skipped} already-passed tests from previous run");
        }
    }

    if cases.is_empty() {
        eprintln!("No tests to run");
        return Ok(ExitCode::SUCCESS);
    }

    eprintln!(
        "Running {} tests ({} jobs, {}s timeout, {} retries)",
        cases.len(),
        config.jobs,
        timeout_secs,
        config.retries,
    );

    let results = run_tests(cases, &config);
    cleanup_containers(&cleanup_executor);
    cleanup_k8s_namespaces(&cleanup_executor);
    let any_failed = results
        .iter()
        .any(|r| matches!(r.outcome, Outcome::Failed | Outcome::TimedOut));

    Ok(if any_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
