// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// Each binary uses a different subset of these helpers, so suppress
// dead_code warnings from the ones it doesn't call.
#![allow(dead_code)]

// `command_ext` is shared with the `rumpelpod` crate via `#[path]` rather
// than a third "util" crate.  Reasons:
//
// * The shared surface is tiny (one trait).
// * A separate crate would need to be published alongside `rumpelpod` if
//   `rumpelpod` is ever released to crates.io, which is unwanted churn for
//   ~150 lines of code.
// * `#[path]` keeps a single source of truth without symlinks (which fare
//   poorly across filesystems and archive tools).
//
// If we ever need to share more than one file, switch to a real `util`
// workspace crate.
#[path = "../../rumpelpod/src/command_ext.rs"]
pub mod command_ext;

pub use command_ext::CommandExt;

pub mod xtest_timeouts;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Cap automatic xtest parallelism without increasing it on smaller hosts.
/// Explicit CLI and environment overrides are applied after this default.
pub fn default_xtest_jobs(available: usize) -> usize {
    available.min(32)
}

/// Build a `cargo` Command with Cargo-injected env vars stripped.
///
/// `cargo run` injects CARGO_PKG_*, CARGO_MANIFEST_DIR, OUT_DIR, etc.
/// into the binary it launches.  Child cargo processes inherit these, and
/// Cargo fingerprints them into dependency build scripts.  On the next
/// invocation (from a shell without those vars) the fingerprints mismatch
/// and large crates recompile.  Strip everything Cargo injects.
pub fn cargo_cmd() -> Command {
    let mut cmd = Command::new("cargo");
    for (key, _) in std::env::vars() {
        let dominated = key.starts_with("CARGO_")
            || key.starts_with("DEP_")
            || matches!(
                key.as_str(),
                "OUT_DIR" | "TARGET" | "HOST" | "NUM_JOBS" | "OPT_LEVEL" | "PROFILE" | "DEBUG"
            );
        if dominated {
            cmd.env_remove(&key);
        }
    }
    cmd
}

/// Parse a key=value state file into a map.
///
/// Blank lines and lines starting with '#' are ignored.
pub fn parse_state_file(path: &Path) -> Result<HashMap<String, String>> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut map = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.to_string(), value.to_string());
        }
    }
    Ok(map)
}

/// Find the repository root via `git rev-parse --show-toplevel`.
pub fn repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running git rev-parse")?;
    if !output.status.success() {
        anyhow::bail!("not inside a git repository");
    }
    let path = String::from_utf8(output.stdout).context("git output is not valid UTF-8")?;
    Ok(PathBuf::from(path.trim()))
}

/// Check that a CLI tool is available on PATH.
pub fn require_tool(name: &str, install_hint: &str) -> Result<()> {
    let status = Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            anyhow::bail!("{name} is not installed.\n{install_hint}");
        }
    }
}

/// Run a command, inheriting stdout/stderr.  Return an error if it fails.
pub fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !status.success() {
        anyhow::bail!("{:?} exited with {}", cmd.get_program(), status);
    }
    Ok(())
}

/// Run a command and capture its stdout as a string.  Stderr is inherited.
pub fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        anyhow::bail!("{:?} exited with {}", cmd.get_program(), out.status);
    }
    Ok(String::from_utf8(out.stdout)
        .context("command output is not valid UTF-8")?
        .trim()
        .to_string())
}

/// Replace an installed binary atomically so a running process keeps its
/// original executable while the next launch observes the new build.
pub fn install_binary(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination.parent().with_context(|| {
        let destination = destination.display();
        format!("installed binary has no parent directory: {destination}")
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        let parent = parent.display();
        format!("creating installed binary directory {parent}")
    })?;

    let staged =
        tempfile::NamedTempFile::new_in(parent).context("creating staged installed binary")?;
    std::fs::copy(source, staged.path()).with_context(|| {
        let source = source.display();
        format!("staging installed binary {source}")
    })?;
    let permissions = std::fs::metadata(source)
        .with_context(|| {
            let source = source.display();
            format!("reading installed binary permissions from {source}")
        })?
        .permissions();
    std::fs::set_permissions(staged.path(), permissions)
        .context("setting installed binary permissions")?;
    staged
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| {
            let destination = destination.display();
            format!("installing binary at {destination}")
        })?;
    Ok(())
}

/// Run a command silently, discarding stdout and stderr.
/// Returns Ok if the command exits successfully, Err otherwise.
pub fn run_quiet(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !status.success() {
        anyhow::bail!("{:?} exited with {}", cmd.get_program(), status);
    }
    Ok(())
}

/// Prompt the user with a yes/no question.  Returns true if they answer y/Y.
pub fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush().context("flushing stderr")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading stdin")?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

pub fn split_xtest_directive_line(line: &str) -> Option<(&str, &str)> {
    const MARKER: &str = "xtest:";

    let trimmed = line.trim_start();
    if let Some(directive) = trimmed.strip_prefix(MARKER) {
        return Some(("", directive.trim()));
    }

    let pos = line.find(MARKER)?;
    let before = &line[..pos];
    let before_trimmed = before.trim();
    if before_trimmed.starts_with("test ") && before_trimmed.ends_with("...") {
        Some((before.trim_end(), line[pos + MARKER.len()..].trim()))
    } else {
        None
    }
}

pub fn is_xtest_prelude_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || (trimmed.starts_with("running ")
            && (trimmed.ends_with(" test") || trimmed.ends_with(" tests")))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{default_xtest_jobs, install_binary, split_xtest_directive_line};

    #[test]
    fn install_binary_atomically_replaces_existing_destination() {
        let directory = tempfile::tempdir().expect("create binary installation directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("bin/rumpel");
        std::fs::write(&source, b"new binary").expect("write source binary");
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o751))
            .expect("set source binary permissions");
        std::fs::create_dir_all(destination.parent().expect("destination parent"))
            .expect("create destination directory");
        std::fs::write(&destination, b"old binary").expect("write existing binary");

        install_binary(&source, &destination).expect("replace installed binary");

        assert_eq!(
            std::fs::read(&destination).expect("read installed binary"),
            b"new binary"
        );
        let mode = std::fs::metadata(&destination)
            .expect("read installed binary metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o751);
    }

    #[test]
    fn default_xtest_jobs_caps_only_large_hosts() {
        assert_eq!(default_xtest_jobs(1), 1);
        assert_eq!(default_xtest_jobs(8), 8);
        assert_eq!(default_xtest_jobs(16), 16);
        assert_eq!(default_xtest_jobs(32), 32);
        assert_eq!(default_xtest_jobs(64), 32);
    }

    #[test]
    fn split_xtest_directive_line_accepts_plain_directive() {
        assert_eq!(
            split_xtest_directive_line("xtest:timeout=145"),
            Some(("", "timeout=145"))
        );
    }

    #[test]
    fn split_xtest_directive_line_accepts_libtest_progress_prefix() {
        assert_eq!(
            split_xtest_directive_line("test codex::smoke::codex_smoke ... xtest:skip"),
            Some(("test codex::smoke::codex_smoke ...", "skip"))
        );
    }

    #[test]
    fn split_xtest_directive_line_ignores_terminal_contents() {
        assert_eq!(
            split_xtest_directive_line("  | println!(\"xtest:timeout=300\")"),
            None
        );
        assert_eq!(
            split_xtest_directive_line("Tests can print xtest:timeout=N before running"),
            None
        );
    }

    #[test]
    fn is_xtest_prelude_line_only_accepts_libtest_header() {
        assert!(super::is_xtest_prelude_line(""));
        assert!(super::is_xtest_prelude_line("running 1 test"));
        assert!(super::is_xtest_prelude_line("running 12 tests"));
        assert!(!super::is_xtest_prelude_line(
            "Tests can print xtest:timeout=N before running"
        ));
    }
}
