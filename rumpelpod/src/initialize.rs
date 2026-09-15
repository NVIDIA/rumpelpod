// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Host-side execution of devcontainer initializeCommand.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::async_command::AsyncCommandExt;
use crate::config::{ContainerEngine, Host};
use crate::devcontainer::{
    collect_local_env_var_names, resolve_devcontainer_vars, DevContainer, LifecycleCommand,
    StringOrArray,
};
use crate::image::OutputLine;

/// Run initializeCommand when configured.
///
/// The client supplies only local environment variables referenced by the
/// devcontainer configuration. Backend overrides are added to that same map
/// before variable substitution so `${localEnv:DOCKER_HOST}` matches the
/// child process and the configuration applied afterward.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_async(
    repo_path: &Path,
    devcontainer_path: Option<&Path>,
    pod_name: &str,
    host: &Host,
    docker_socket: Option<&Path>,
    ssh_auth_sock: Option<&Path>,
    local_env: &mut HashMap<String, String>,
    progress: &std::sync::mpsc::Sender<OutputLine>,
) -> Result<()> {
    let raw_devcontainer_json =
        DevContainer::find_raw(repo_path, devcontainer_path)?.unwrap_or_else(|| "{}".to_string());
    let mut remove_env: HashSet<String> = collect_local_env_var_names(&raw_devcontainer_json)
        .into_iter()
        .filter(|name| !local_env.contains_key(name))
        .collect();
    let mut backend_environment = backend_environment(host, docker_socket)?;
    match ssh_auth_sock {
        Some(socket) => {
            backend_environment.set.insert(
                "SSH_AUTH_SOCK".to_string(),
                socket.to_string_lossy().into_owned(),
            );
        }
        None => {
            backend_environment
                .remove
                .insert("SSH_AUTH_SOCK".to_string());
        }
    }
    for name in backend_environment.remove {
        local_env.remove(&name);
        remove_env.insert(name);
    }
    for (name, value) in backend_environment.set {
        remove_env.remove(&name);
        local_env.insert(name, value);
    }

    let devcontainer = DevContainer::find_and_load(repo_path, devcontainer_path)?
        .map(|(devcontainer, _)| devcontainer)
        .unwrap_or_default();
    let Some(command) = devcontainer.initialize_command.as_ref() else {
        return Ok(());
    };
    if command_is_empty(command) {
        return Ok(());
    }

    let devcontainer = resolve_devcontainer_vars(devcontainer, repo_path, pod_name, local_env);
    let Some(command) = devcontainer.initialize_command.as_ref() else {
        return Err(anyhow::anyhow!(
            "initializeCommand disappeared during variable substitution"
        ));
    };

    let _ = progress.send(OutputLine::Stderr(
        "running initializeCommand...".to_string(),
    ));
    run_command(command, repo_path, local_env, &remove_env, progress)
        .await
        .context("initializeCommand in devcontainer.json failed")?;
    Ok(())
}

fn command_is_empty(command: &LifecycleCommand) -> bool {
    match command {
        LifecycleCommand::String(command) => command.trim().is_empty(),
        LifecycleCommand::Array(command) => command.is_empty(),
        LifecycleCommand::Object(command) => command.is_empty(),
    }
}

fn backend_environment(host: &Host, docker_socket: Option<&Path>) -> Result<EnvironmentOverrides> {
    let mut set = HashMap::new();
    let mut remove = HashSet::new();
    match host {
        Host::Localhost {
            engine: ContainerEngine::Docker,
        } => {
            let socket = docker_socket.context("local Docker socket is unavailable")?;
            let socket = socket.display();
            set.insert("DOCKER_HOST".to_string(), format!("unix://{socket}"));
            remove.insert("DOCKER_CONTEXT".to_string());
        }
        Host::Ssh {
            ssh_destination,
            engine: ContainerEngine::Docker,
        } => {
            set.insert(
                "DOCKER_HOST".to_string(),
                format!("ssh://{ssh_destination}"),
            );
            remove.insert("DOCKER_CONTEXT".to_string());
        }
        Host::Ssh {
            ssh_destination,
            engine: ContainerEngine::Podman,
        } => {
            let host = format!("ssh://{ssh_destination}");
            set.insert("DOCKER_HOST".to_string(), host.clone());
            set.insert("CONTAINER_HOST".to_string(), host);
            remove.insert("DOCKER_CONTEXT".to_string());
            remove.insert("CONTAINER_CONNECTION".to_string());
        }
        Host::Localhost {
            engine: ContainerEngine::Podman,
        } => {
            remove.insert("DOCKER_HOST".to_string());
            remove.insert("DOCKER_CONTEXT".to_string());
            remove.insert("CONTAINER_HOST".to_string());
            remove.insert("CONTAINER_CONNECTION".to_string());
        }
        Host::Kubernetes { .. } => {}
        Host::Localhost {
            engine: ContainerEngine::Auto,
        }
        | Host::Ssh {
            engine: ContainerEngine::Auto,
            ..
        } => panic!("container engine auto remained after resolve"),
    }
    Ok(EnvironmentOverrides { set, remove })
}

struct EnvironmentOverrides {
    set: HashMap<String, String>,
    remove: HashSet<String>,
}

async fn run_command(
    command: &LifecycleCommand,
    workdir: &Path,
    env: &HashMap<String, String>,
    remove_env: &HashSet<String>,
    progress: &std::sync::mpsc::Sender<OutputLine>,
) -> Result<()> {
    match command {
        LifecycleCommand::String(command) => {
            let args = ["/bin/sh".to_string(), "-c".to_string(), command.clone()];
            run_one(
                "initializeCommand",
                &args,
                workdir,
                env,
                remove_env,
                progress,
            )
            .await
        }
        LifecycleCommand::Array(args) => {
            run_one(
                "initializeCommand",
                args,
                workdir,
                env,
                remove_env,
                progress,
            )
            .await
        }
        LifecycleCommand::Object(commands) => {
            run_parallel(commands, workdir, env, remove_env, progress).await
        }
    }
}

async fn run_parallel(
    commands: &HashMap<String, StringOrArray>,
    workdir: &Path,
    env: &HashMap<String, String>,
    remove_env: &HashSet<String>,
    progress: &std::sync::mpsc::Sender<OutputLine>,
) -> Result<()> {
    let pending: Vec<_> = commands
        .iter()
        .map(|(name, command)| {
            let name = name.clone();
            let args = match command {
                StringOrArray::String(command) => {
                    vec!["/bin/sh".to_string(), "-c".to_string(), command.clone()]
                }
                StringOrArray::Array(args) => args.clone(),
            };
            let workdir = workdir.to_path_buf();
            let env = env.clone();
            let remove_env = remove_env.clone();
            let progress = progress.clone();
            async move {
                let label = format!("initializeCommand/{name}");
                run_one(&label, &args, &workdir, &env, &remove_env, &progress).await
            }
        })
        .collect();

    let mut first_error = None;
    for result in futures_util::future::join_all(pending).await {
        if let Err(error) = result {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn run_one(
    name: &str,
    args: &[String],
    workdir: &Path,
    env: &HashMap<String, String>,
    remove_env: &HashSet<String>,
    progress: &std::sync::mpsc::Sender<OutputLine>,
) -> Result<()> {
    let Some((program, args)) = args.split_first() else {
        return Ok(());
    };

    // Regular files let a shell start a background tunnel without keeping the
    // initializer blocked on inherited output pipes.
    let stdout = tempfile::NamedTempFile::new().context("creating initializer stdout buffer")?;
    let stderr = tempfile::NamedTempFile::new().context("creating initializer stderr buffer")?;
    let mut command = Command::new(program);
    command.args(args).current_dir(workdir).envs(env);
    for name in remove_env {
        command.env_remove(name);
    }
    let status = command
        .stdout(Stdio::from(
            stdout
                .reopen()
                .context("opening initializer stdout buffer")?,
        ))
        .stderr(Stdio::from(
            stderr
                .reopen()
                .context("opening initializer stderr buffer")?,
        ))
        .status_async()
        .await
        .with_context(|| format!("spawning {name}"))?;

    let stdout_len = stdout
        .as_file()
        .metadata()
        .context("reading initializer stdout metadata")?
        .len();
    let stderr_len = stderr
        .as_file()
        .metadata()
        .context("reading initializer stderr metadata")?
        .len();
    emit_output(stdout.path(), stdout_len, OutputStream::Stdout, progress)?;
    emit_output(stderr.path(), stderr_len, OutputStream::Stderr, progress)?;

    if !status.success() {
        return Err(anyhow::anyhow!("{name} exited with {status}"));
    }
    Ok(())
}

enum OutputStream {
    Stdout,
    Stderr,
}

fn emit_output(
    path: &Path,
    length: u64,
    stream: OutputStream,
    progress: &std::sync::mpsc::Sender<OutputLine>,
) -> Result<()> {
    let file = File::open(path).context("opening initializer output")?;
    let mut reader = file.take(length);
    let mut input = [0_u8; 8192];
    let mut line = Vec::new();
    loop {
        let count = reader
            .read(&mut input)
            .context("reading initializer output")?;
        if count == 0 {
            break;
        }
        let mut remaining = &input[..count];
        while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&remaining[..newline]);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !send_output_line(&line, &stream, progress) {
                return Ok(());
            }
            line.clear();
            remaining = &remaining[newline + 1..];
        }
        line.extend_from_slice(remaining);
        if line.len() >= 64 * 1024 {
            if !send_output_line(&line, &stream, progress) {
                return Ok(());
            }
            line.clear();
        }
    }
    if !line.is_empty() {
        send_output_line(&line, &stream, progress);
    }
    Ok(())
}

fn send_output_line(
    line: &[u8],
    stream: &OutputStream,
    progress: &std::sync::mpsc::Sender<OutputLine>,
) -> bool {
    let line = String::from_utf8_lossy(line).into_owned();
    let line = match stream {
        OutputStream::Stdout => OutputLine::Stdout(line),
        OutputStream::Stderr => OutputLine::Stderr(line),
    };
    progress.send(line).is_ok()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::backend_environment;
    use crate::config::{ContainerEngine, Host};

    #[test]
    fn initialize_command_ssh_environment_targets_remote_engine() {
        let host = Host::Ssh {
            ssh_destination: "developer@example.test".to_string(),
            engine: ContainerEngine::Docker,
        };

        let environment = backend_environment(&host, None).expect("resolve SSH environment");
        assert_eq!(
            environment.set.get("DOCKER_HOST").map(String::as_str),
            Some("ssh://developer@example.test")
        );
        assert!(environment.remove.contains("DOCKER_CONTEXT"));
    }

    #[test]
    fn initialize_command_kubernetes_environment_does_not_invent_docker_target() {
        let host = Host::Kubernetes {
            context: "test-context".to_string(),
            namespace: "test-namespace".to_string(),
            registry: "registry.example.test".to_string(),
            node_selector: None,
            tolerations: None,
            builder: None,
            image_builder: ContainerEngine::Docker,
        };

        let environment = backend_environment(&host, None).expect("resolve Kubernetes environment");
        assert!(environment.set.is_empty());
        assert!(environment.remove.is_empty());
    }

    #[test]
    fn initialize_command_local_docker_environment_uses_daemon_socket() {
        let host = Host::Localhost {
            engine: ContainerEngine::Docker,
        };

        let environment = backend_environment(&host, Some(Path::new("/tmp/daemon-docker.sock")))
            .expect("resolve Docker environment");
        assert_eq!(
            environment.set.get("DOCKER_HOST").map(String::as_str),
            Some("unix:///tmp/daemon-docker.sock")
        );
        assert!(environment.remove.contains("DOCKER_CONTEXT"));
    }

    #[test]
    fn initialize_command_local_podman_environment_clears_other_targets() {
        let host = Host::Localhost {
            engine: ContainerEngine::Podman,
        };

        let environment = backend_environment(&host, None).expect("resolve Podman environment");
        assert!(environment.set.is_empty());
        assert!(environment.remove.contains("DOCKER_HOST"));
        assert!(environment.remove.contains("DOCKER_CONTEXT"));
        assert!(environment.remove.contains("CONTAINER_HOST"));
        assert!(environment.remove.contains("CONTAINER_CONNECTION"));
    }
}
