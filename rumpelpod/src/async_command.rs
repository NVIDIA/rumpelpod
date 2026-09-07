// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Output, Stdio};

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::image::OutputLine;

// Killing just the shell leaves its children running after startup is
// cancelled. Keep the group owned until the whole command has completed.
pub(crate) struct ProcessGroup(Option<Pid>);

impl ProcessGroup {
    pub(crate) fn new(child: &tokio::process::Child) -> Self {
        Self(child.id().map(|id| Pid::from_raw(id as i32)))
    }

    pub(crate) fn completed(&mut self, status: ExitStatus) {
        if status.success() {
            self.0 = None;
        }
    }
}

// Restore the builder even when a future is cancelled or spawning fails.
struct CommandLease<'a> {
    target: &'a mut Command,
    command: Option<tokio::process::Command>,
}

impl<'a> CommandLease<'a> {
    fn new(target: &'a mut Command) -> Self {
        let command = take_command(target);
        Self {
            target,
            command: Some(command),
        }
    }
}

impl std::ops::Deref for CommandLease<'_> {
    type Target = tokio::process::Command;
    fn deref(&self) -> &Self::Target {
        self.command.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for CommandLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.command.as_mut().unwrap()
    }
}

impl Drop for CommandLease<'_> {
    fn drop(&mut self) {
        *self.target = self.command.take().unwrap().into_std();
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            match killpg(pid, Signal::SIGKILL) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(e) => eprintln!("failed to kill cancelled command group {pid}: {e}"),
            }
        }
    }
}

fn take_command(command: &mut Command) -> tokio::process::Command {
    let mut command = tokio::process::Command::from(std::mem::replace(command, Command::new("")));
    command.as_std_mut().process_group(0);
    command.kill_on_drop(true);
    command
}

// These methods use Tokio's stdio semantics: output captures stdout/stderr
// and leaves stdin as configured. Callers feeding no input can set it to null.
pub(crate) trait AsyncCommandExt {
    async fn output_async(&mut self) -> io::Result<Output>;
    async fn status_async(&mut self) -> io::Result<ExitStatus>;
    async fn success_async(&mut self) -> Result<Vec<u8>>;
}

impl AsyncCommandExt for Command {
    async fn output_async(&mut self) -> io::Result<Output> {
        let mut command = CommandLease::new(self);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let mut group = ProcessGroup::new(&child);
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let (status, _, _) = tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut stdout_bytes),
            stderr.read_to_end(&mut stderr_bytes),
        )?;
        group.completed(status);
        Ok(Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        })
    }

    async fn status_async(&mut self) -> io::Result<ExitStatus> {
        let mut command = CommandLease::new(self);
        let mut child = command.spawn()?;
        let mut group = ProcessGroup::new(&child);
        let status = child.wait().await?;
        group.completed(status);
        Ok(status)
    }

    async fn success_async(&mut self) -> Result<Vec<u8>> {
        let description = format!("{self:?}");
        let output = self.output_async().await?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let status = output.status;
            return Err(anyhow::anyhow!(
                "$ {description}\n{stdout}{stderr}\n{status}"
            ));
        }
        Ok(output.stdout)
    }
}

pub(crate) async fn stream_output(
    command: &mut Command,
    mut on_output: impl FnMut(OutputLine) + Send,
) -> Result<()> {
    let mut child_command = CommandLease::new(command);
    child_command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = child_command.spawn().context("starting command")?;
    let mut group = ProcessGroup::new(&child);
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout was piped")).lines();
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr was piped")).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut output = String::new();
    while !stdout_done || !stderr_done {
        tokio::select! {
            line = stdout.next_line(), if !stdout_done => match line? {
                Some(line) => {
                    output.push_str(&line);
                    output.push('\n');
                    on_output(OutputLine::Stdout(line));
                }
                None => stdout_done = true,
            },
            line = stderr.next_line(), if !stderr_done => match line? {
                Some(line) => {
                    output.push_str(&line);
                    output.push('\n');
                    on_output(OutputLine::Stderr(line));
                }
                None => stderr_done = true,
            },
        }
    }
    let status = child.wait().await?;
    group.completed(status);
    if !status.success() {
        return Err(anyhow::anyhow!("command failed ({status}):\n{output}"));
    }
    Ok(())
}
