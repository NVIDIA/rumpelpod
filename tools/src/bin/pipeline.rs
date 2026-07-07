// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Local CI pipeline: format, build, lint, test, and check git status.
//!
//! Usage: cargo pipeline [--continue] [--release] [args for cargo xtest...]
//!
//! Test output is recorded to `target/xtest/<timestamp>.log`.
//! Use `--continue` to resume a previous run, skipping tests that
//! already passed.
//! Use `--release` to build and test the optimized binaries (CI does
//! this); the default dev build is faster to compile.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result};
use chrono::Local;

/// Tees output to both stderr and a log file.
struct Log {
    file: std::fs::File,
}

impl Log {
    fn new(path: &Path) -> Result<Self> {
        let file = std::fs::File::create(path)
            .with_context(|| format!("creating log file {}", path.display()))?;
        Ok(Self { file })
    }

    fn writeln(&mut self, msg: &str) {
        eprintln!("{msg}");
        let _ = writeln!(self.file, "{msg}");
    }

    /// Run a command, teeing its combined stdout+stderr to both
    /// stderr and the log file.  Returns an error if the command
    /// fails.
    fn run(&mut self, cmd: &mut Command) -> Result<()> {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("running {:?}", cmd.get_program()))?;

        // Merge stdout and stderr into a single stream by reading
        // both in background threads.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, rx) = std::sync::mpsc::channel::<String>();

        let tx2 = tx.clone();
        let stdout_thread = std::thread::spawn(move || {
            if let Some(out) = stdout {
                for line in BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    let _ = tx2.send(line);
                }
            }
        });
        let tx3 = tx;
        let stderr_thread = std::thread::spawn(move || {
            if let Some(err) = stderr {
                for line in BufReader::new(err).lines() {
                    let Ok(line) = line else { break };
                    let _ = tx3.send(line);
                }
            }
        });

        // Print lines as they arrive.
        for line in rx {
            eprintln!("{line}");
            let _ = writeln!(self.file, "{line}");
        }

        stdout_thread.join().unwrap();
        stderr_thread.join().unwrap();

        let status = child.wait().with_context(|| {
            let prog = cmd.get_program().to_string_lossy().to_string();
            format!("waiting for {prog}")
        })?;
        if !status.success() {
            let prog = cmd.get_program().to_string_lossy().to_string();
            anyhow::bail!("{prog} exited with {status}");
        }
        Ok(())
    }
}

/// Run a command with inherited stdio, failing if it exits non-zero.
fn run_cmd(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !status.success() {
        let prog = cmd.get_program().to_string_lossy().to_string();
        anyhow::bail!("{prog} exited with {status}");
    }
    Ok(())
}

/// Find the most recent .log file in `target/xtest/`.
fn most_recent_log(dir: &Path) -> Result<PathBuf> {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .collect();
    // Filenames are timestamps, so lexicographic sort gives
    // chronological order.
    logs.sort();
    logs.pop()
        .with_context(|| format!("no log files found in {}", dir.display()))
}

/// Parse a pipeline log and return test names that passed or were
/// skipped.
///
/// Looks for xtest progress lines like:
///   [  1/42] some::test_name ... ok (1.2s)
///   [  1/42] some::test_name ... ok, 1 retry (1.2s)
///   [  1/42] some::test_name ... SKIPPED (0.0s)
fn passed_tests_from_log(path: &Path) -> Result<HashSet<String>> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut names = HashSet::new();
    for line in content.lines() {
        let line = line.trim();
        // Progress lines start with "[" and contain "..."
        if !line.starts_with('[') {
            continue;
        }
        let Some(after_bracket) = line.split(']').nth(1) else {
            continue;
        };
        let after_bracket = after_bracket.trim();
        // Format: "test_name ... status (Ns)"
        let Some((test_name, rest)) = after_bracket.split_once(" ... ") else {
            continue;
        };
        if rest.starts_with("ok") || rest.starts_with("SKIPPED") {
            names.insert(test_name.to_string());
        }
    }
    Ok(names)
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
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let continue_mode = if let Some(pos) = args.iter().position(|a| a == "--continue") {
        args.remove(pos);
        true
    } else {
        false
    };

    // Consumed here rather than passed through so it also applies to
    // the build and clippy steps, then re-added for xtest below.
    let release = if let Some(pos) = args.iter().position(|a| a == "--release") {
        args.remove(pos);
        true
    } else {
        false
    };

    let log_dir = PathBuf::from("target/xtest");
    std::fs::create_dir_all(&log_dir).context("creating target/xtest")?;

    // Collect passed tests from the previous run before creating a new
    // log file (otherwise we would pick up the empty new file).
    let skip_names = if continue_mode {
        let prev = most_recent_log(&log_dir)?;
        let names = passed_tests_from_log(&prev)?;
        let prev_display = prev.display();
        eprintln!(
            "Continuing from {prev_display} ({} passed tests to skip)",
            names.len()
        );
        Some(names)
    } else {
        None
    };

    eprintln!("=== Checking formatting ===");
    run_cmd(tools::cargo_cmd().args(["fmt", "--", "--check"]))?;

    let profile_args: &[&str] = if release { &["--release"] } else { &[] };

    eprintln!("\n=== Building (with tests, no warnings) ===");
    run_cmd(
        tools::cargo_cmd()
            .args(["build", "--all-targets"])
            .args(profile_args)
            .env("RUSTFLAGS", "-D warnings"),
    )?;

    eprintln!("\n=== Running clippy (no warnings) ===");
    run_cmd(
        tools::cargo_cmd()
            .args(["clippy", "--all-targets"])
            .args(profile_args)
            .env("RUSTFLAGS", "-D warnings"),
    )?;

    // Only xtest output is recorded to the log file.  The other
    // pipeline steps are cheap and deterministic; the log exists so
    // --continue can identify which tests already passed.
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let log_path = log_dir.join(format!("{timestamp}.log"));
    let mut log = Log::new(&log_path)?;

    let log_path_display = log_path.display();
    eprintln!("\n=== Running tests (log: {log_path_display}) ===");

    let mut xtest_cmd = tools::cargo_cmd();
    xtest_cmd.arg("xtest");
    if release {
        xtest_cmd.arg("--release");
    }

    // --skip-file must come before positional args because clap's
    // trailing_var_arg treats everything after the first positional
    // as passthrough.
    if let Some(names) = skip_names {
        if !names.is_empty() {
            let skip_file = log_dir.join("continue-skip.txt");
            std::fs::write(&skip_file, names.into_iter().collect::<Vec<_>>().join("\n"))
                .context("writing skip file")?;
            xtest_cmd.arg("--skip-file").arg(&skip_file);
        }
    }

    xtest_cmd.args(&args);

    let test_result = log.run(&mut xtest_cmd);
    if let Err(e) = test_result {
        log.writeln(&format!("\n=== Tests failed: {e} ==="));
        return Ok(ExitCode::FAILURE);
    }

    eprintln!("\n=== Checking git status ===");
    let dirty = !Command::new("git")
        .args(["diff", "--quiet"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        || !Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    let untracked =
        tools::output(Command::new("git").args(["ls-files", "--others", "--exclude-standard"]))
            .unwrap_or_default();

    if dirty || !untracked.is_empty() {
        eprintln!("WARNING: Working directory is not clean!");
        eprintln!();
        eprintln!("Modified or staged files:");
        run_cmd(Command::new("git").args(["status", "--short"]))?;
        if !untracked.is_empty() {
            eprintln!();
            eprintln!("Untracked files:");
            eprintln!("{untracked}");
        }
    } else {
        eprintln!("Working directory is clean");
    }

    eprintln!("\n=== Pipeline complete ===");
    Ok(ExitCode::SUCCESS)
}
