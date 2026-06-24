// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod async_runtime;
mod claude;
mod cli;
mod codex;
mod command_ext;
pub(crate) mod completions;
pub mod config;
mod container_exec;
mod cp;
mod devcontainer;
mod exec_proxy;
mod executor;

pub use command_ext::CommandExt;
pub mod daemon;
mod delete;
mod enter;
mod fork;
pub mod gateway;
mod git;
mod git_http_server;
mod grok;
mod hook;
mod hub;
pub(crate) mod image;
mod image_cmd;
mod k8s;
mod list;
mod llm;
mod merge;
mod pi;
mod pod;
mod port_file;
mod ports;
mod prepared_image;
mod prune;
mod pty_attach;
mod pty_session;
mod recreate;
mod registry;
mod review;
mod service;
mod slow_guard;
mod ssh;
mod stop;
mod switch_user;
mod tcp_proxy;
mod tunnel;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

/// Resolve a binary by name on PATH, like which(1).
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn ensure_tui_terminal_env() {
    // TERM=dumb is common for daemon/test parents, but TUIs treat it
    // as non-interactive even when we have allocated a usable terminal.
    let term = std::env::var_os("TERM");
    if term.is_none() || term.as_deref() == Some(OsStr::new("dumb")) {
        std::env::set_var("TERM", "xterm-256color");
    }
    if std::env::var_os("COLORTERM").is_none() {
        std::env::set_var("COLORTERM", "truecolor");
    }
}

/// Whether an operation should retry indefinitely (user can cancel) or
/// give up after a sensible limit (no user waiting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryPolicy {
    /// A user is waiting and can cancel (e.g. Ctrl-C).  Retry
    /// indefinitely, reporting errors so the user knows what is happening.
    UserBlocking,
    /// No user is waiting.  Retry with sensible limits determined by the
    /// retry logic itself.
    Background,
}

/// Apply jitter to a delay so concurrent retriers don't stampede.
///
/// Returns a uniformly random duration in [delay/2, delay].
pub(crate) fn jitter(d: Duration) -> Duration {
    d.mul_f64(0.5 + rand::random::<f64>() * 0.5)
}

use cli::{
    ClaudeHookSubcommand, Cli, Command, GitHookSubcommand, HubSubcommand, ImageSubcommand, ShellArg,
};

pub fn run() -> Result<()> {
    env_logger::init();
    completions::complete();
    let cli = Cli::parse();

    match cli.command {
        Command::Daemon => {
            daemon::run_daemon()?;
        }
        Command::TunnelServer => {
            tunnel::run_tunnel_server();
        }
        Command::TcpProxy { port } => {
            tcp_proxy::run_tcp_proxy(port);
        }
        Command::ContainerExec { command, workdir } => {
            container_exec::container_exec(command, workdir)?;
        }
        Command::ContainerServe {
            ref token,
            ref repo_path,
            ref pod_name,
            ref local_env,
            ref git_setup_spec,
            test_mode,
        } => {
            let setup_params = match git_setup_spec {
                Some(json) => Some(
                    serde_json::from_str::<pod::GitSetupParams>(json)
                        .context("parsing --git-setup-spec")?,
                ),
                None => None,
            };
            pod::run_container_server(
                token.clone(),
                repo_path.clone(),
                pod_name.clone(),
                local_env.clone(),
                setup_params,
                test_mode,
            );
        }
        Command::SystemInstall => {
            service::system_install()?;
        }
        Command::SystemUninstall => {
            service::system_uninstall()?;
        }
        Command::Enter(ref cmd) => {
            enter::enter(cmd)?;
        }
        Command::List => {
            list::list()?;
        }
        Command::Stop(ref cmd) => {
            stop::stop(cmd)?;
        }
        Command::Delete(ref cmd) => {
            delete::delete(cmd)?;
        }
        Command::Prune(ref cmd) => {
            prune::prune(cmd)?;
        }
        Command::Ports(ref cmd) => {
            ports::ports(cmd)?;
        }
        Command::ForwardPort(ref cmd) => {
            ports::forward_port(cmd)?;
        }
        Command::Recreate(ref cmd) => {
            recreate::recreate(cmd)?;
        }
        Command::Fork(ref cmd) => {
            fork::fork(cmd)?;
        }
        Command::Image(ref sub) => match sub {
            ImageSubcommand::Build(ref cmd) => image_cmd::build(cmd)?,
            ImageSubcommand::Fetch(ref cmd) => image_cmd::fetch(cmd)?,
        },
        Command::Review(ref cmd) => {
            review::review(cmd)?;
        }
        Command::Merge(ref cmd) => {
            merge::merge(cmd)?;
        }
        Command::Cp(ref cmd) => {
            cp::cp(cmd)?;
        }
        Command::Claude(ref cmd) => match cmd.action {
            Some(cli::ClaudeAction::Reauth) => {
                claude::reauth(cmd)?;
            }
            None => {
                claude::claude(cmd)?;
            }
        },
        Command::Codex(ref cmd) => {
            codex::codex(cmd)?;
        }
        Command::Pi(ref cmd) => {
            pi::pi(cmd)?;
        }
        Command::Grok(ref cmd) => {
            grok::grok(cmd)?;
        }
        Command::SshAdd(ref cmd) => {
            ssh::ssh_add(cmd)?;
        }
        Command::PrepareImage(ref cmd) => {
            prepared_image::run_prepare_image(cmd)?;
        }
        Command::Completions(ref cmd) => {
            let shell = match cmd.shell {
                ShellArg::Bash => "bash",
                ShellArg::Fish => "fish",
                ShellArg::Zsh => "zsh",
            };
            completions::generate(shell);
        }
        Command::GitHook(ref sub) => match sub {
            GitHookSubcommand::ReferenceTransaction(ref cmd) => {
                hook::reference_transaction(cmd)?;
            }
            GitHookSubcommand::HostPreReceive => {
                hook::host_pre_receive()?;
            }
            GitHookSubcommand::PreCommitDescription(ref cmd) => {
                hook::pre_commit_description(cmd)?;
            }
        },
        Command::ClaudeHook(ref sub) => match sub {
            ClaudeHookSubcommand::PermissionRequest => {
                hook::claude_permission_request()?;
            }
            ClaudeHookSubcommand::NotifyState(ref args) => {
                hook::claude_notify_state(&args.state)?;
            }
        },
        Command::Hub(ref sub) => match sub {
            HubSubcommand::Install(ref args) => {
                let host = hub::resolve_hub_host(args)?;
                hub::install(&host)?;
            }
            HubSubcommand::Delete(ref args) => {
                let host = hub::resolve_hub_host(args)?;
                hub::delete(&host)?;
            }
            HubSubcommand::Status(ref args) => {
                let host = hub::resolve_hub_host(args)?;
                hub::status(&host)?;
            }
            HubSubcommand::Serve => {
                hub::run_serve()?;
            }
        },
    }

    Ok(())
}
