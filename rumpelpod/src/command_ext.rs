// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Extension trait for `std::process::Command` that provides combined stdout/stderr output.
//!
//! This module allows capturing both stdout and stderr from a child process into a single
//! interleaved string, preserving the order in which output was produced.

use std::io::{self, pipe, BufRead, BufReader};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;

use anyhow::{anyhow, Context, Result};
use log::{error, info};

/// The output of a command with combined stdout and stderr.
#[derive(Debug)]
pub struct CombinedOutput {
    /// The combined stdout and stderr output as a string.
    pub combined_output: String,
    /// The exit status of the process (same field name as `std::process::Output`).
    pub status: ExitStatus,
}

/// Extension trait for `Command` that provides combined output functionality.
pub trait CommandExt {
    /// Executes the command and captures stdout and stderr into a single combined string.
    ///
    /// Both stdout and stderr are written to the same pipe, so the output is interleaved
    /// in the order it was produced by the process. The output is read line by line and
    /// expects valid UTF-8.
    fn combined_output(&mut self) -> io::Result<CombinedOutput>;

    /// Executes the command and returns stdout if it exits successfully.
    ///
    /// On failure, returns an error with the command, combined output, and exit status.
    fn success(&mut self) -> Result<Vec<u8>>;

    /// Spawns the command and logs its stdout/stderr to the provided callbacks.
    ///
    /// The callbacks are executed in separate threads.
    #[doc(hidden)]
    fn spawn_with_logging(&mut self, prefix: &str) -> Result<Child>;
}

impl CommandExt for Command {
    fn combined_output(&mut self) -> io::Result<CombinedOutput> {
        let (reader, writer) = pipe()?;

        // Clone the writer so we can give one to stdout and one to stderr.
        let writer_clone = writer.try_clone()?;

        let mut child = self.stdout(writer).stderr(writer_clone).spawn()?;

        // The Command struct holds onto the Stdio file descriptors internally.
        // We must close our copies so the pipe can reach EOF when the child exits.
        // Setting to Stdio::null() replaces (and closes) the previous descriptors.
        self.stdout(Stdio::null());
        self.stderr(Stdio::null());

        // Read line by line from the combined pipe.
        // The read will complete once the child closes both stdout and stderr, typically when it
        // exits.
        let mut combined_output = String::new();
        let buf_reader = BufReader::new(reader);
        for line in buf_reader.lines() {
            let line = line?;
            combined_output.push_str(&line);
            combined_output.push('\n');
        }

        let status = child.wait()?;

        Ok(CombinedOutput {
            combined_output,
            status,
        })
    }

    fn success(&mut self) -> Result<Vec<u8>> {
        let output = self.output()?;

        if output.status.success() {
            Ok(output.stdout)
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_info = match output.status.code() {
                Some(code) => format!("exit code: {code}"),
                None => "killed by signal".to_string(),
            };
            Err(anyhow!("$ {self:?}\n{stdout}{stderr}{exit_info}"))
        }
    }

    fn spawn_with_logging(&mut self, prefix: &str) -> Result<Child> {
        self.stdout(Stdio::piped());
        self.stderr(Stdio::piped());

        let mut child = self.spawn().context("failed to spawn process")?;

        let prefix = prefix.to_string();

        let stdout = child.stdout.take().expect("stdout not captured");
        let thread_name = format!("{prefix}-stdout");
        let prefix_clone = prefix.clone();
        thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            if !line.trim().is_empty() {
                                info!("{prefix_clone}: {line}");
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .with_context(|| format!("failed to spawn {thread_name} thread"))?;

        let stderr = child.stderr.take().expect("stderr not captured");
        let thread_name = format!("{prefix}-stderr");
        thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            if !line.trim().is_empty() {
                                error!("{prefix}: {line}");
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .with_context(|| format!("failed to spawn {thread_name} thread"))?;

        Ok(child)
    }
}
