// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use log::{info, trace};

use crate::cli::EnterCommand;
use crate::config::{load_json_config, Host};
use crate::daemon;
use crate::daemon::protocol::{
    Daemon, DaemonClient, LaunchProgress, LaunchResult, PodLaunchParams, PodName,
};
use crate::devcontainer::{DevContainer, GpuRequirement, HostRequirements, SubstitutionContext};
use crate::git::{get_current_branch, get_git_user_config, get_repo_root};
use crate::image::OutputLine;

/// Compute the path relative from `base` to `path`.
/// Both paths must be absolute and `path` must be under `base`.
fn relative_path<'a>(base: &Path, path: &'a Path) -> Result<&'a Path> {
    path.strip_prefix(base).with_context(|| {
        let path = path.display();
        let base = base.display();
        format!("{path} is not under {base}")
    })
}

/// Map the local machine's current directory into the corresponding container path.
fn container_workdir(container_repo_path: &Path, repo_root: &Path) -> Result<PathBuf> {
    let current_dir = std::env::current_dir().context("Failed to get current directory")?;
    let relative = relative_path(repo_root, &current_dir)?;
    Ok(container_repo_path.join(relative))
}

/// Parse a human-readable size string (e.g. "4gb", "512MB") into bytes.
///
/// Supports "tb", "gb", "mb", "kb" suffixes (case-insensitive).
/// Returns None if the string cannot be parsed.
pub fn parse_size_string(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();

    let suffixes: &[(&str, u64)] = &[
        ("tb", 1024 * 1024 * 1024 * 1024),
        ("gb", 1024 * 1024 * 1024),
        ("mb", 1024 * 1024),
        ("kb", 1024),
    ];

    for (suffix, multiplier) in suffixes {
        if let Some(num_str) = s.strip_suffix(suffix) {
            return num_str.trim().parse::<u64>().ok().map(|n| n * multiplier);
        }
    }

    // No suffix -- treat as raw bytes.
    s.parse::<u64>().ok()
}

/// Log hostRequirements and decide whether to enforce them based on host type.
fn check_host_requirements(requirements: &HostRequirements, docker_host: &Host) {
    let mut parts: Vec<String> = Vec::new();

    if let Some(cpus) = requirements.cpus {
        parts.push(format!("cpus={cpus}"));
    }
    if let Some(ref memory) = requirements.memory {
        match parse_size_string(memory) {
            Some(bytes) => parts.push(format!("memory={memory} ({bytes} bytes)")),
            None => parts.push(format!("memory={memory} (unparseable)")),
        }
    }
    if let Some(ref storage) = requirements.storage {
        match parse_size_string(storage) {
            Some(bytes) => parts.push(format!("storage={storage} ({bytes} bytes)")),
            None => parts.push(format!("storage={storage} (unparseable)")),
        }
    }
    if let Some(ref gpu) = requirements.gpu {
        match gpu {
            GpuRequirement::Required(true) => parts.push("gpu=required".to_string()),
            GpuRequirement::Required(false) => {}
            GpuRequirement::Optional(s) => parts.push(format!("gpu={s}")),
            GpuRequirement::Detailed(details) => parts.push(format!("gpu={details:?}")),
        }
    }

    if parts.is_empty() {
        return;
    }

    let parts = parts.join(", ");
    info!("hostRequirements: {parts}");

    // Localhost and remote Docker: the user controls their hardware, so we
    // only log.  A future orchestrator backend (e.g. Kubernetes) should match
    // here and use the parsed values to select an appropriate node or instance
    // type.
    match docker_host {
        Host::Localhost | Host::Ssh { .. } => {
            info!("Running on local/remote Docker -- hostRequirements are advisory only");
        }
        Host::Kubernetes { .. } => {
            info!("Running on Kubernetes -- hostRequirements set as pod resource requests");
        }
    }
}

/// Determine the target Docker host from CLI override or .rumpelpod.json.
pub fn determine_host(repo_root: &Path, host_override: Option<Host>) -> Result<Host> {
    if let Some(h) = host_override {
        return Ok(h);
    }
    let json_config = load_json_config(repo_root)?;
    if let Some(ref host_str) = json_config.host {
        return Host::parse(host_str).context("Invalid host in .rumpelpod.json");
    }
    if let Some(ref kubernetes) = json_config.kubernetes {
        return Ok(Host::Kubernetes {
            context: kubernetes.context.clone(),
            namespace: kubernetes
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            registry: kubernetes.registry.clone(),
            node_selector: kubernetes.node_selector.clone(),
            tolerations: kubernetes.tolerations.clone(),
            builder: kubernetes.builder.clone(),
        });
    }
    Ok(Host::Localhost)
}

/// Collect `${localEnv:VAR}` values from the local environment so the
/// daemon can substitute them when it loads devcontainer.json.
///
/// The daemon cannot do this itself because it does not have access to
/// the user's shell environment.  This is the only reason the client
/// opens devcontainer.json at all.
pub fn collect_local_env(repo_root: &Path) -> Result<HashMap<String, String>> {
    let raw = DevContainer::find_raw(repo_root)?.unwrap_or_else(|| "{}".to_string());
    Ok(crate::devcontainer::collect_local_env_vars(&raw))
}

/// Client-side helper for commands that build or pull an image outside
/// the daemon (`rumpel image build` / `rumpel image fetch`).
///
/// Unlike `prepare_launch_inputs`, this returns a fully-resolved
/// `DevContainer` and host so those commands can call `image::resolve_image`
/// directly.  Launch/recreate paths must not use this -- they ship the
/// work to the daemon instead.
pub fn load_for_image_cmd(
    repo_root: &Path,
    host_override: Option<Host>,
) -> Result<(DevContainer, Host)> {
    let docker_host = determine_host(repo_root, host_override)?;

    let (mut devcontainer, devcontainer_dir) = DevContainer::find_and_load(repo_root)?
        .unwrap_or_else(|| (DevContainer::default(), repo_root.to_path_buf()));

    devcontainer.resolve_build_paths(&devcontainer_dir, repo_root);

    let local_env_vars = collect_local_env(repo_root)?;

    let local_ws = repo_root
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    let local_ws_basename = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    devcontainer = devcontainer.substitute(&SubstitutionContext {
        local_env: Some(local_env_vars),
        local_workspace_folder: Some(local_ws),
        local_workspace_folder_basename: Some(local_ws_basename),
        // devcontainer id and container-side paths are not known outside
        // a launch; image build/pull only touches build-time fields so
        // this is fine.
        container_workspace_folder: None,
        container_workspace_folder_basename: None,
        devcontainer_id: None,
    });

    devcontainer
        .resolve_env_files(repo_root)
        .context("resolving --env-file from runArgs")?;

    if let Some(ref requirements) = devcontainer.host_requirements {
        check_host_requirements(requirements, &docker_host);
    }

    Ok((devcontainer, docker_host))
}

/// Resolve the absolute path to the `claude` CLI binary on the local
/// machine.
///
/// The client resolves this so the daemon does not depend on its own
/// PATH (which may be limited, e.g. under systemd).
pub fn find_local_claude_cli() -> Option<PathBuf> {
    crate::which("claude")
}

/// Resolve the absolute path to the `codex` CLI binary on the local
/// machine.  See `find_local_claude_cli` for why the client resolves
/// this rather than the daemon.
pub fn find_local_codex_cli() -> Option<PathBuf> {
    crate::which("codex")
}

/// Resolve the absolute path to the `pi` CLI binary on the local
/// machine.  See `find_local_claude_cli` for why the client resolves
/// this rather than the daemon.
pub fn find_local_pi_cli() -> Option<PathBuf> {
    crate::which("pi")
}

/// Resolve the absolute path to the `grok` CLI binary on the local
/// machine.  See `find_local_claude_cli` for why the client resolves
/// this rather than the daemon.
pub fn find_local_grok_cli() -> Option<PathBuf> {
    crate::which("grok")
}

/// Check whether a pod needs to be created and handle confirmation.
///
/// If the pod already exists, returns Ok immediately.
/// If `create` is true (--create flag), returns Ok so launch_pod will
/// create it without prompting.
/// Otherwise prompts on a terminal, or errors if not on a terminal.
pub fn confirm_pod_creation(pod_name: &str, repo_root: &Path, create: bool) -> Result<()> {
    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);
    let pods = client.list_pods(repo_root.to_path_buf())?;
    if pods.iter().any(|p| p.name == pod_name) {
        return Ok(());
    }

    if create {
        return Ok(());
    }

    if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        return Err(anyhow::anyhow!(
            "pod '{pod_name}' does not exist (use --create to create it)"
        ));
    }

    eprint!("Pod '{pod_name}' does not exist. Create it? [Y/n] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    if answer.is_empty() || answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err(anyhow::anyhow!("pod creation cancelled"))
    }
}

/// Launch a pod and return the container ID and user.
pub fn launch_pod(pod_name: &str, host_override: Option<Host>) -> Result<LaunchResult> {
    let t = Instant::now();
    let repo_root = get_repo_root()?;
    let docker_host = determine_host(&repo_root, host_override)?;
    let local_env_vars = collect_local_env(&repo_root)?;
    let elapsed = t.elapsed();
    trace!("launch_pod config: {elapsed:?}");

    let host_branch = get_current_branch(&repo_root);
    let git_identity = get_git_user_config(&repo_root);
    let claude_cli_path = find_local_claude_cli();
    let codex_cli_path = find_local_codex_cli();
    let pi_cli_path = find_local_pi_cli();
    let grok_cli_path = find_local_grok_cli();
    let json_config = load_json_config(&repo_root)?;

    let socket_path = daemon::socket_path()?;
    let client = DaemonClient::new_unix(&socket_path);

    let t = Instant::now();
    let description_file = json_config
        .merge
        .description_file_path()
        .map(str::to_string);
    let ssh_auth_sock = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    let pod_name = PodName::new(pod_name.to_string()).map_err(|e| anyhow::anyhow!(e))?;
    let mut progress = client.launch_pod(PodLaunchParams {
        pod_name,
        repo_path: repo_root,
        host_branch,
        host: docker_host,
        git_identity: Some(git_identity),
        claude_cli_path,
        codex_cli_path,
        pi_cli_path,
        inject_system_prompt: json_config.inject_system_prompt,
        grok_cli_path,
        description_file,
        local_env_vars,
        ssh_auth_sock,
    })?;
    for line in &mut progress {
        match line {
            OutputLine::Stdout(s) => println!("{s}"),
            OutputLine::Stderr(s) => eprintln!("{s}"),
        }
    }
    let result = progress.finish()?;
    let elapsed = t.elapsed();
    trace!("launch_pod daemon RPC: {elapsed:?}");

    Ok(result)
}

pub fn enter(cmd: &EnterCommand) -> Result<()> {
    let t_total = Instant::now();

    let t = Instant::now();
    let repo_root = get_repo_root()?;
    let elapsed = t.elapsed();
    trace!("get_repo_root: {elapsed:?}");

    let host_override = cmd.host_args.resolve()?;

    confirm_pod_creation(&cmd.name, &repo_root, cmd.create)?;

    let t = Instant::now();
    let result = launch_pod(&cmd.name, host_override)?;
    let elapsed = t.elapsed();
    trace!("launch_pod: {elapsed:?}");

    let container_repo_path = result.container_repo_path.clone();
    let workdir = container_workdir(&container_repo_path, &repo_root)?;

    // Wrap in `rumpel container-exec` so the pod server's resolved
    // environment is applied inside the container without passing
    // env vars on the docker/kubectl exec command line.
    // When no command is given, container-exec defaults to the user's
    // login shell with flags from the baked devcontainer.json.
    let workdir_str = workdir.to_string_lossy();
    let mut exec_cmd = vec![
        crate::daemon::RUMPEL_CONTAINER_BIN.to_string(),
        "container-exec".to_string(),
        "--workdir".to_string(),
        workdir_str.to_string(),
        "--".to_string(),
    ];
    exec_cmd.extend(cmd.command.clone());

    // Local Docker reuses the socket the daemon resolved before it
    // entered the test or user runtime environment.  SSH and
    // Kubernetes connect through their native client transports.
    let executor = match &result.host {
        Host::Localhost => {
            let socket = result
                .docker_socket
                .as_ref()
                .context("docker_socket is required for localhost Docker")?;
            crate::executor::Executor::docker(socket)?
        }
        Host::Ssh { .. } => crate::executor::Executor::docker_host(&result.host)?,
        Host::Kubernetes {
            context, namespace, ..
        } => crate::executor::Executor::kubernetes(context, namespace)?,
    };
    let pod_id = crate::executor::PodId::new(result.container_id.0.clone())
        .map_err(|e| anyhow::anyhow!(e))?;

    // The host subdir may not exist in the container yet.  Docker only;
    // on k8s the emptyDir mounts come up owned by the container user so
    // arbitrary mkdir without --user root is ambiguous, and in practice
    // none of the k8s callers hit this path.
    if matches!(result.host, Host::Localhost | Host::Ssh { .. }) && workdir != container_repo_path {
        let mkdir_status = executor
            .exec_interactive(
                &pod_id,
                &[
                    "mkdir".to_string(),
                    "-p".to_string(),
                    workdir.to_string_lossy().to_string(),
                ],
                crate::executor::ExecInteractiveOptions {
                    tty: false,
                    user_root: true,
                },
            )
            .context("Failed to create workdir in container")?;
        if !mkdir_status.success() {
            let workdir = workdir.display();
            return Err(anyhow::anyhow!(
                "Failed to create workdir {workdir} in container"
            ));
        }
    }

    // Exec as root on docker; the in-pod rumpel container-exec switches
    // to the container user (from /opt/rumpelpod/user) before running
    // the actual command.  Kubernetes has no --user override so the
    // executor ignores user_root there and enters as the image USER.
    let status = executor.exec_interactive(
        &pod_id,
        &exec_cmd,
        crate::executor::ExecInteractiveOptions {
            tty: std::io::stdin().is_terminal(),
            user_root: true,
        },
    )?;

    let elapsed = t_total.elapsed();
    trace!("total enter startup: {elapsed:?}");

    if !status.success() {
        return Err(anyhow::anyhow!("exec exited with status {status}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_gb() {
        assert_eq!(parse_size_string("4gb"), Some(4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_size_case_insensitive() {
        assert_eq!(parse_size_string("512MB"), Some(512 * 1024 * 1024));
    }

    #[test]
    fn parse_size_tb() {
        assert_eq!(
            parse_size_string("2tb"),
            Some(2 * 1024 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_size_kb() {
        assert_eq!(parse_size_string("128kb"), Some(128 * 1024));
    }

    #[test]
    fn parse_size_raw_bytes() {
        assert_eq!(parse_size_string("1048576"), Some(1048576));
    }

    #[test]
    fn parse_size_invalid() {
        assert_eq!(parse_size_string("not-a-number"), None);
    }

    #[test]
    fn parse_size_whitespace() {
        assert_eq!(parse_size_string(" 8 gb "), Some(8 * 1024 * 1024 * 1024));
    }
}
