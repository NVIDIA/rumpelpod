// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `Executor` itself.
//!
//! Dispatches backend-agnostic pod ops to a docker or kubernetes
//! implementation.  Callers interact only with this type; the
//! backend variant is an internal detail.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;

use crate::async_runtime::block_on;
use crate::config::Host;
use crate::daemon::default_docker_socket;
use crate::daemon::host_connection::HostConnection;
use crate::daemon::protocol::PodStatus;
use crate::k8s::K8sClient;
use crate::CommandExt;

use super::spec::MountType;
use super::{
    ExecOutput, ExecRequest, ExecStreams, PodId, PodSpec, LABEL_DOCKER_POD_NAME,
    LABEL_DOCKER_REPO_PATH, LABEL_K8S_POD_NAME, LABEL_K8S_REPO_HASH,
};

#[derive(Clone)]
pub struct Executor {
    inner: Inner,
}

#[derive(Clone)]
enum Inner {
    Docker(DockerBackend),
    Kubernetes(K8sBackend),
}

#[derive(Clone)]
struct DockerBackend {
    cli_target: DockerCliTarget,
}

#[derive(Clone)]
struct K8sBackend {
    client: K8sClient,
}

#[derive(Clone)]
enum DockerCliTarget {
    UnixSocket(std::path::PathBuf),
    Ssh { uri: String },
}

impl Executor {
    /// Connect to a docker daemon over a unix socket.
    pub fn docker(socket: &std::path::Path) -> Result<Self> {
        Ok(Self {
            inner: Inner::Docker(DockerBackend {
                cli_target: DockerCliTarget::UnixSocket(socket.to_path_buf()),
            }),
        })
    }

    /// Connect to a docker daemon through Docker's SSH transport.
    pub fn docker_ssh(ssh_destination: &str) -> Result<Self> {
        let uri = format!("ssh://{ssh_destination}");
        Ok(Self {
            inner: Inner::Docker(DockerBackend {
                cli_target: DockerCliTarget::Ssh { uri },
            }),
        })
    }

    /// Connect to a Docker host without going through the daemon's
    /// `HostConnection` registry.  Used by CLI-side interactive exec
    /// after the daemon has already launched or reconnected the pod.
    pub fn docker_host(host: &Host) -> Result<Self> {
        match host {
            Host::Localhost => Self::docker(&default_docker_socket()),
            Host::Ssh { ssh_destination } => Self::docker_ssh(ssh_destination),
            Host::Kubernetes { .. } => {
                panic!("docker_host called on Kubernetes host")
            }
        }
    }

    /// Connect to a kubernetes cluster via kubeconfig.
    pub fn kubernetes(context: &str, namespace: &str) -> Result<Self> {
        Ok(Self {
            inner: Inner::Kubernetes(K8sBackend {
                client: K8sClient::new(context, namespace)?,
            }),
        })
    }

    /// Build an executor for a given host connection.
    ///
    /// Verifies the host connection before constructing the backend
    /// client.  Failure surfaces to the caller so commands like
    /// `rumpel enter` can report the underlying transport error
    /// directly.
    pub fn new(conn: &HostConnection) -> Result<Self> {
        match conn {
            HostConnection::Localhost(_) => Self::docker(&default_docker_socket()),
            HostConnection::Ssh(ssh) => {
                ssh.ensure_connected()
                    .context("opening ssh docker transport")?;
                Self::docker_ssh(ssh.destination())
            }
            HostConnection::Kubernetes(k) => {
                let client = k.ensure_client().context("opening k8s connection")?;
                Ok(Self {
                    inner: Inner::Kubernetes(K8sBackend { client }),
                })
            }
        }
    }

    /// Create the pod, start it, and wait until it is running.
    ///
    /// On kubernetes this polls for the `Running` phase, surfacing
    /// container-level errors (e.g. ImagePullBackOff).  On docker,
    /// `docker start` is synchronous and no wait is needed.
    pub fn launch(&self, id: &PodId, spec: PodSpec) -> Result<()> {
        match &self.inner {
            Inner::Docker(d) => {
                if !spec.k8s_only.is_empty() {
                    anyhow::bail!(
                        "k8s-only spec fields (node_selector/tolerations) set on a docker launch"
                    );
                }
                docker_launch(d, id, spec)
            }
            Inner::Kubernetes(k) => {
                if !spec.docker_only.is_empty() {
                    anyhow::bail!(
                        "docker-only spec fields (init/devices/network/security_opt/port_bindings) \
                         set on a kubernetes launch"
                    );
                }
                k8s_launch(k, id, spec)
            }
        }
    }

    /// Remove a pod.  Idempotent: succeeds if the pod is already gone.
    pub fn delete(&self, id: &PodId) -> Result<()> {
        match &self.inner {
            Inner::Docker(d) => docker_delete(d, id),
            Inner::Kubernetes(k) => k.client.delete_pod(id.as_str()),
        }
    }

    /// Current pod status.  Returns `Gone` when the pod no longer
    /// exists on the backend.
    pub fn status(&self, id: &PodId) -> Result<PodStatus> {
        match &self.inner {
            Inner::Docker(d) => docker_status(d, id),
            Inner::Kubernetes(k) => k.client.get_pod_status(id.as_str()),
        }
    }

    /// Run a command inside the pod, wait for it to finish, and
    /// collect output.  Enters as the image's USER on both backends;
    /// there is no override, matching k8s's constraint.
    pub fn exec(&self, id: &PodId, req: ExecRequest) -> Result<ExecOutput> {
        block_on(self.exec_async(id, req))
    }

    /// Async variant of [`Self::exec`].  Use from within a tokio task
    /// so the call doesn't re-enter the shared runtime.
    pub async fn exec_async(&self, id: &PodId, req: ExecRequest) -> Result<ExecOutput> {
        match &self.inner {
            Inner::Docker(d) => docker_exec(d, id, req).await,
            Inner::Kubernetes(k) => k8s_exec(k, id, req).await,
        }
    }

    /// Start a command inside the pod without waiting for it to
    /// finish or collecting output.  Used for long-lived in-pod
    /// servers (e.g. container-serve).  Docker uses `detach: true`;
    /// kubernetes fakes it by backgrounding under `sh -c`, which
    /// means stdin/stdout/stderr are all discarded.
    pub fn exec_detached(&self, id: &PodId, req: ExecRequest) -> Result<()> {
        match &self.inner {
            Inner::Docker(d) => docker_exec_detached(d, id, req),
            Inner::Kubernetes(k) => k8s_exec_detached(k, id, req),
        }
    }

    /// Start a command inside the pod and return split stdin/stdout/stderr
    /// streams.  The session stays live until the returned `ExecStreams`
    /// is dropped.
    pub async fn exec_streaming(&self, id: &PodId, cmd: Vec<String>) -> Result<ExecStreams> {
        match &self.inner {
            Inner::Docker(d) => docker_exec_streaming(d, id, cmd).await,
            Inner::Kubernetes(k) => k8s_exec_streaming(k, id, cmd).await,
        }
    }

    /// Stop a running pod without removing it.  Errors on
    /// kubernetes, which has no analogue -- the only way to stop
    /// a k8s pod is to delete it.
    pub fn stop(&self, id: &PodId) -> Result<()> {
        match &self.inner {
            Inner::Docker(d) => docker_stop(d, id),
            Inner::Kubernetes(_) => {
                anyhow::bail!("stop not supported on kubernetes, delete the pod instead")
            }
        }
    }

    /// Start a stopped pod.  Errors on kubernetes for the same
    /// reason as `stop`.
    pub fn start(&self, id: &PodId) -> Result<()> {
        match &self.inner {
            Inner::Docker(d) => docker_start(d, id),
            Inner::Kubernetes(_) => {
                anyhow::bail!("start not supported on kubernetes, launch a new pod instead")
            }
        }
    }

    /// Whether `image` is present on the backend.
    ///
    /// On kubernetes the cluster's image inventory is not globally
    /// visible to the client, so this is always `Ok(true)`: callers
    /// using this as a preflight against a missing fork image trust
    /// the cluster's pull behavior and let any failure surface at
    /// pod start.
    pub fn image_present(&self, image: &str) -> Result<bool> {
        match &self.inner {
            Inner::Docker(d) => docker_image_present(d, image),
            Inner::Kubernetes(_) => Ok(true),
        }
    }

    /// Interactive exec that inherits the caller's stdio.
    ///
    /// Shells out to `docker` or `kubectl` because native CLIs handle
    /// TTY wiring for stdin/stdout.  Blocks until the remote process exits.
    ///
    /// `opts.user_root` is docker-only; kubernetes has no analogue and
    /// silently ignores it (matching how `Executor::exec` handles
    /// image USER).
    pub fn exec_interactive(
        &self,
        id: &PodId,
        cmd: &[String],
        opts: ExecInteractiveOptions,
    ) -> Result<std::process::ExitStatus> {
        match &self.inner {
            Inner::Docker(d) => docker_exec_interactive(d, id, cmd, opts),
            Inner::Kubernetes(k) => k8s_exec_interactive(k, id, cmd, opts),
        }
    }

    /// Enumerate rumpelpod-managed pods for a given repo path.
    ///
    /// Filters by a backend-specific repo label (full path on docker,
    /// repo-hash on kubernetes).  Returns a map keyed by logical pod
    /// name (from the `rumpelpod-name` label) with the pod's status
    /// and backend identifier.  Pods without the name label are
    /// skipped rather than reported as unnamed.
    pub fn list_by_repo(&self, repo_path: &Path) -> Result<HashMap<String, PodBackendInfo>> {
        match &self.inner {
            Inner::Docker(d) => docker_list_by_repo(d, repo_path),
            Inner::Kubernetes(k) => k8s_list_by_repo(k, repo_path),
        }
    }
}

/// Info about a single pod returned by [`Executor::list_by_repo`].
#[derive(Debug, Clone)]
pub struct PodBackendInfo {
    pub status: PodStatus,
    /// Backend's identifier for the pod: docker container id on docker,
    /// pod name on kubernetes.  Used when callers shell out to
    /// `docker`/`kubectl` (e.g. interactive exec).
    pub container_id: String,
}

/// Options for [`Executor::exec_interactive`].
#[derive(Default, Clone, Copy)]
pub struct ExecInteractiveOptions {
    /// Allocate a pseudo-TTY (docker's/kubectl's `-t`).  Set when the
    /// caller's own stdin is a terminal.
    pub tty: bool,
    /// Enter the container as root (docker's `--user root`).  No
    /// kubectl analogue; the executor enters as the image USER there.
    pub user_root: bool,
}

impl DockerBackend {
    fn command(&self) -> Command {
        let mut command = Command::new("docker");
        self.apply_target(&mut command);
        command
    }

    fn tokio_command(&self) -> TokioCommand {
        let mut command = TokioCommand::new("docker");
        self.apply_tokio_target(&mut command);
        command
    }

    fn apply_target(&self, command: &mut Command) {
        match &self.cli_target {
            DockerCliTarget::UnixSocket(socket) => {
                let socket = socket.display().to_string();
                command.args(["-H", &format!("unix://{socket}")]);
            }
            DockerCliTarget::Ssh { uri } => {
                command.args(["-H", uri]);
            }
        }
    }

    fn apply_tokio_target(&self, command: &mut TokioCommand) {
        match &self.cli_target {
            DockerCliTarget::UnixSocket(socket) => {
                let socket = socket.display().to_string();
                command.args(["-H", &format!("unix://{socket}")]);
            }
            DockerCliTarget::Ssh { uri } => {
                command.args(["-H", uri]);
            }
        }
    }
}

#[derive(Deserialize)]
struct DockerContainerInspect {
    #[serde(rename = "Id")]
    id: Option<String>,
    #[serde(rename = "Config")]
    config: Option<DockerContainerConfig>,
    #[serde(rename = "State")]
    state: Option<DockerContainerState>,
}

#[derive(Deserialize)]
struct DockerContainerConfig {
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct DockerContainerState {
    #[serde(rename = "Running")]
    running: Option<bool>,
}

fn docker_not_found(output: &std::process::Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such container")
        || stderr.contains("no such image")
        || stderr.contains("no such object")
        || stderr.contains("not found")
}

fn docker_stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

fn docker_inspect_container(
    backend: &DockerBackend,
    id: &PodId,
) -> Result<Option<DockerContainerInspect>> {
    let mut command = backend.command();
    let output = command
        .args(["container", "inspect", id.as_str()])
        .output()
        .context("running docker container inspect")?;
    if !output.status.success() {
        if docker_not_found(&output) {
            return Ok(None);
        }
        let stderr = docker_stderr(&output);
        return Err(anyhow::anyhow!("docker container inspect failed: {stderr}"));
    }

    let mut containers: Vec<DockerContainerInspect> =
        serde_json::from_slice(&output.stdout).context("parsing docker container inspect")?;
    if containers.is_empty() {
        return Err(anyhow::anyhow!(
            "docker container inspect succeeded without returning a container"
        ));
    }
    Ok(Some(containers.remove(0)))
}

fn docker_exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn docker_delete(backend: &DockerBackend, id: &PodId) -> Result<()> {
    let mut command = backend.command();
    let output = command
        .args(["rm", "-f", id.as_str()])
        .output()
        .context("running docker rm")?;
    if output.status.success() || docker_not_found(&output) {
        return Ok(());
    }
    let stderr = docker_stderr(&output);
    Err(anyhow::anyhow!("docker rm failed: {stderr}"))
}

fn docker_status(backend: &DockerBackend, id: &PodId) -> Result<PodStatus> {
    let Some(container) = docker_inspect_container(backend, id)? else {
        return Ok(PodStatus::Gone);
    };
    let state = container
        .state
        .context("docker container inspect response missing State")?;
    let running = state.running.unwrap_or(false);
    if running {
        Ok(PodStatus::Running)
    } else {
        Ok(PodStatus::Stopped)
    }
}

async fn docker_exec(backend: &DockerBackend, id: &PodId, req: ExecRequest) -> Result<ExecOutput> {
    let mut command = backend.tokio_command();
    command.arg("exec");
    if req.stdin.is_some() {
        command.arg("-i");
    }
    if let Some(workdir) = req.workdir.as_ref() {
        command.args(["--workdir", workdir]);
    }
    for (key, value) in &req.env {
        command.arg("--env");
        command.arg(format!("{key}={value}"));
    }
    command.arg(id.as_str());
    command.args(&req.cmd);
    if req.stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().context("spawning docker exec")?;
    if let Some(data) = req.stdin {
        let mut stdin = child.stdin.take().context("docker exec stdin missing")?;
        stdin
            .write_all(&data)
            .await
            .context("writing docker exec stdin")?;
        stdin
            .shutdown()
            .await
            .context("closing docker exec stdin")?;
    }

    let output = child
        .wait_with_output()
        .await
        .context("waiting for docker exec")?;
    Ok(ExecOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: docker_exit_code(output.status),
    })
}

async fn k8s_exec(backend: &K8sBackend, id: &PodId, req: ExecRequest) -> Result<ExecOutput> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, AttachParams};
    use tokio::io::AsyncReadExt;

    if req.workdir.is_some() {
        // kube-rs exec has no workdir parameter.  Callers that need it
        // today wrap the command in `sh -c "cd ... && ..."`; spell that
        // out rather than silently ignoring the field.
        anyhow::bail!(
            "executor::exec workdir is not supported on kubernetes, \
             wrap the command in `sh -c 'cd ... && ...'` instead"
        );
    }
    if !req.env.is_empty() {
        anyhow::bail!(
            "executor::exec env is not supported on kubernetes, \
             use `env VAR=val ... cmd` or the pod's baked env instead"
        );
    }

    let pods: Api<Pod> =
        Api::namespaced(backend.client.client().clone(), backend.client.namespace());

    let mut attached = pods
        .exec(
            id.as_str(),
            req.cmd,
            &AttachParams::default()
                .stdout(true)
                .stderr(true)
                .stdin(req.stdin.is_some()),
        )
        .await
        .context("exec in pod")?;

    if let Some(data) = req.stdin {
        use tokio::io::AsyncWriteExt;
        if let Some(mut w) = attached.stdin() {
            w.write_all(&data).await.context("writing exec stdin")?;
            w.shutdown().await.context("closing exec stdin")?;
        }
    }

    let mut stdout_buf = Vec::new();
    if let Some(mut stdout) = attached.stdout() {
        stdout.read_to_end(&mut stdout_buf).await?;
    }
    let mut stderr_buf = Vec::new();
    if let Some(mut stderr) = attached.stderr() {
        stderr.read_to_end(&mut stderr_buf).await?;
    }

    let status = attached
        .take_status()
        .unwrap()
        .await
        .context("waiting for exec status")?;

    let exit_code = parse_k8s_exit_code(&status)?;
    Ok(ExecOutput {
        stdout: stdout_buf,
        stderr: stderr_buf,
        exit_code,
    })
}

async fn docker_exec_streaming(
    backend: &DockerBackend,
    id: &PodId,
    cmd: Vec<String>,
) -> Result<ExecStreams> {
    let mut command = backend.tokio_command();
    command.arg("exec");
    command.arg("-i");
    command.arg(id.as_str());
    command.args(cmd);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

    let mut child = command.spawn().context("spawning streaming docker exec")?;
    let stdin = child.stdin.take().context("taking docker exec stdin")?;
    let stdout = child.stdout.take().context("taking docker exec stdout")?;
    let stderr = child.stderr.take().context("taking docker exec stderr")?;

    Ok(ExecStreams {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        keepalive: Box::new(child),
    })
}

async fn k8s_exec_streaming(
    backend: &K8sBackend,
    id: &PodId,
    cmd: Vec<String>,
) -> Result<ExecStreams> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, AttachParams};

    let pods: Api<Pod> =
        Api::namespaced(backend.client.client().clone(), backend.client.namespace());

    let mut attached = pods
        .exec(
            id.as_str(),
            cmd,
            &AttachParams::default()
                .stdin(true)
                .stdout(true)
                .stderr(true),
        )
        .await
        .context("exec in pod")?;

    let stdin = attached.stdin().context("taking exec stdin")?;
    let stdout = attached.stdout().context("taking exec stdout")?;
    let stderr = attached.stderr().context("taking exec stderr")?;

    Ok(ExecStreams {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        keepalive: Box::new(attached),
    })
}

fn docker_launch(backend: &DockerBackend, id: &PodId, spec: PodSpec) -> Result<()> {
    let PodSpec {
        image,
        hostname,
        cmd,
        env,
        mounts,
        labels,
        annotations: _,
        privileged,
        cap_add,
        seccomp_unconfined,
        apparmor_unconfined,
        resources: _,
        runtime,
        docker_only,
        k8s_only: _,
    } = spec;

    let mut security_opt = docker_only.security_opt.clone();
    if seccomp_unconfined {
        security_opt.push("seccomp=unconfined".into());
    }
    if apparmor_unconfined {
        security_opt.push("apparmor=unconfined".into());
    }

    let mut command = backend.command();
    command.arg("create");
    command.args(["--name", id.as_str()]);
    command.args(["--hostname", hostname.as_str()]);
    command.args([
        "--network",
        docker_only.network.as_deref().unwrap_or("bridge"),
    ]);
    if let Some(runtime) = runtime {
        command.args(["--runtime", &runtime]);
    }
    if privileged {
        command.arg("--privileged");
    }
    if docker_only.init {
        command.arg("--init");
    }
    for cap in cap_add {
        command.arg("--cap-add");
        command.arg(cap);
    }
    for security_opt in security_opt {
        command.arg("--security-opt");
        command.arg(security_opt);
    }
    for device in docker_only.devices {
        command.arg("--device");
        command.arg(device);
    }
    for (container_port, host_port) in docker_only.port_bindings {
        command.arg("--publish");
        command.arg(format!("127.0.0.1:{host_port}:{container_port}/tcp"));
    }
    for mount in mounts {
        command.arg("--mount");
        command.arg(docker_mount_arg(mount));
    }
    for (key, value) in labels {
        command.arg("--label");
        command.arg(format!("{key}={value}"));
    }
    for (key, value) in env {
        command.arg("--env");
        command.arg(format!("{key}={value}"));
    }
    command.arg(image);
    if let Some(cmd) = cmd {
        command.args(cmd);
    }

    command.success().context("creating container")?;
    docker_start(backend, id).context("starting container")?;
    Ok(())
}

fn docker_mount_arg(mount: super::Mount) -> String {
    let typ = match mount.mount_type {
        MountType::Bind => "bind",
        MountType::Volume => "volume",
        MountType::Tmpfs => "tmpfs",
    };

    let mut parts = vec![format!("type={typ}")];
    if let Some(source) = mount.source {
        parts.push(format!("source={source}"));
    }
    parts.push(format!("target={}", mount.target));
    if mount.read_only {
        parts.push("readonly".to_string());
    }
    parts.join(",")
}

fn k8s_launch(backend: &K8sBackend, id: &PodId, spec: PodSpec) -> Result<()> {
    use crate::k8s::{K8sPodOptions, K8sResourceRequests, K8sVolumeMount};

    let PodSpec {
        image,
        hostname,
        cmd,
        env,
        mounts,
        labels,
        annotations,
        privileged,
        cap_add,
        seccomp_unconfined,
        apparmor_unconfined,
        resources,
        runtime,
        docker_only: _,
        k8s_only,
    } = spec;

    let volumes: Vec<K8sVolumeMount> = mounts
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let medium = match m.mount_type {
                MountType::Tmpfs => Some("Memory".to_string()),
                // K8s emptyDir volumes don't honor bind sources.  The
                // existing launch_pod_k8s path only passes tmpfs-style
                // mounts, so Bind/Volume here degrade to disk-backed
                // emptyDir.  Docker-style bind mounts have no k8s
                // analogue without hostPath, which the cluster admin
                // has to allow.
                MountType::Bind | MountType::Volume => None,
            };
            K8sVolumeMount {
                name: format!("vol-{i}"),
                mount_path: m.target.clone(),
                read_only: m.read_only,
                medium,
            }
        })
        .collect();

    let options = K8sPodOptions {
        volumes,
        privileged,
        cap_add,
        seccomp_unconfined,
        apparmor_unconfined,
        hostname: Some(hostname.as_str().to_string()),
        cmd,
        resource_requests: resources.map(|r| K8sResourceRequests {
            cpu: r.cpu,
            memory: r.memory,
        }),
        node_selector: k8s_only.node_selector,
        tolerations: k8s_only.tolerations,
        // K8s semantics: `None` means the cluster default (runc).
        // Callers pass `None` rather than `Some("runc")`; normalize
        // anyway for defense-in-depth.
        runtime_class_name: runtime.filter(|r| r != "runc"),
    };

    backend
        .client
        .create_pod(id.as_str(), &image, labels, annotations, &env, &options)?;

    backend.client.wait_running(id.as_str())?;
    Ok(())
}

fn docker_exec_detached(backend: &DockerBackend, id: &PodId, req: ExecRequest) -> Result<()> {
    if req.stdin.is_some() {
        return Err(anyhow::anyhow!(
            "executor::exec_detached does not support stdin on docker"
        ));
    }

    let mut command = backend.command();
    command.args(["exec", "-d"]);
    if let Some(workdir) = req.workdir.as_ref() {
        command.args(["--workdir", workdir]);
    }
    for (key, value) in &req.env {
        command.arg("--env");
        command.arg(format!("{key}={value}"));
    }
    command.arg(id.as_str());
    command.args(req.cmd);
    command.success().context("starting detached docker exec")?;
    Ok(())
}

fn k8s_exec_detached(backend: &K8sBackend, id: &PodId, req: ExecRequest) -> Result<()> {
    // kube-rs exec has no detach flag.  Approximate it by wrapping
    // the command under `sh -c '<cmd> </dev/null >/tmp/exec.log 2>&1 &'`,
    // which returns as soon as sh backgrounds the child.  Output is
    // lost.  Callers that need it should arrange their own logging
    // inside the backgrounded command.
    if req.workdir.is_some() || !req.env.is_empty() || req.stdin.is_some() {
        anyhow::bail!(
            "executor::exec_detached does not support workdir, env, or stdin on kubernetes, \
             inline them into the command itself"
        );
    }
    let escaped: Vec<String> = req
        .cmd
        .iter()
        .map(|a| crate::devcontainer::shell_escape(a))
        .collect();
    let joined = escaped.join(" ");
    let wrapped = format!("{joined} </dev/null >/tmp/rumpel-detached.log 2>&1 &");
    let req = ExecRequest {
        cmd: vec!["sh".into(), "-c".into(), wrapped],
        workdir: None,
        env: Vec::new(),
        stdin: None,
    };
    let out = block_on(k8s_exec(backend, id, req))?;
    if out.exit_code != 0 {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow::anyhow!(
            "detached exec wrapper exited with {}: {stderr}",
            out.exit_code,
        ));
    }
    Ok(())
}

fn docker_stop(backend: &DockerBackend, id: &PodId) -> Result<()> {
    let mut command = backend.command();
    let output = command
        .args(["stop", "--time", "0", id.as_str()])
        .output()
        .context("running docker stop")?;
    if output.status.success() || docker_not_found(&output) {
        return Ok(());
    }
    let stderr = docker_stderr(&output);
    Err(anyhow::anyhow!("docker stop failed: {stderr}"))
}

fn docker_start(backend: &DockerBackend, id: &PodId) -> Result<()> {
    let mut command = backend.command();
    command
        .args(["start", id.as_str()])
        .success()
        .context("starting container")?;
    Ok(())
}

fn docker_exec_interactive(
    backend: &DockerBackend,
    id: &PodId,
    cmd: &[String],
    opts: ExecInteractiveOptions,
) -> Result<std::process::ExitStatus> {
    // `docker exec` treats `--` as a literal argument, so the command
    // follows the container id directly, unlike kubectl.
    let mut c = backend.command();
    c.arg("exec");
    if opts.user_root {
        c.args(["--user", "root"]);
    }
    c.arg("-i");
    if opts.tty {
        c.arg("-t");
    }
    c.arg(id.as_str());
    c.args(cmd);
    c.status().context("spawning docker exec")
}

fn k8s_exec_interactive(
    backend: &K8sBackend,
    id: &PodId,
    cmd: &[String],
    opts: ExecInteractiveOptions,
) -> Result<std::process::ExitStatus> {
    // user_root is ignored: kubectl exec has no --user override, so
    // the session always enters as the image USER.
    let _ = opts.user_root;

    let mut c = Command::new("kubectl");
    c.args(["--context", backend.client.context()]);
    c.args(["--namespace", backend.client.namespace()]);
    c.arg("exec");
    if opts.tty {
        c.arg("-it");
    } else {
        c.arg("-i");
    }
    c.arg(id.as_str());
    c.arg("--");
    c.args(cmd);
    c.status().context("spawning kubectl exec")
}

fn docker_image_present(backend: &DockerBackend, image: &str) -> Result<bool> {
    let mut command = backend.command();
    let output = command
        .args(["image", "inspect", image])
        .output()
        .context("running docker image inspect")?;
    if output.status.success() {
        Ok(true)
    } else if docker_not_found(&output) {
        Ok(false)
    } else {
        let stderr = docker_stderr(&output);
        Err(anyhow::anyhow!("docker image inspect failed: {stderr}"))
    }
}

fn docker_list_by_repo(
    backend: &DockerBackend,
    repo_path: &Path,
) -> Result<HashMap<String, PodBackendInfo>> {
    let label_filter = format!("{LABEL_DOCKER_REPO_PATH}={}", repo_path.display());
    let mut list = backend.command();
    let stdout = list
        .args(["container", "ls", "--all", "--quiet", "--filter"])
        .arg(format!("label={label_filter}"))
        .success()
        .context("listing containers")?;

    let ids: Vec<String> = String::from_utf8_lossy(&stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut inspect = backend.command();
    let stdout = inspect
        .args(["container", "inspect"])
        .args(&ids)
        .success()
        .context("inspecting listed containers")?;
    let containers: Vec<DockerContainerInspect> =
        serde_json::from_slice(&stdout).context("parsing docker container inspect")?;

    let mut out = HashMap::new();
    for container in containers {
        let labels = container
            .config
            .and_then(|config| config.labels)
            .unwrap_or_default();
        let pod_name = match labels.get(LABEL_DOCKER_POD_NAME) {
            Some(n) => n.clone(),
            None => continue,
        };
        let state = container
            .state
            .context("docker container inspect response missing State")?;
        let status = if state.running.unwrap_or(false) {
            PodStatus::Running
        } else {
            PodStatus::Stopped
        };
        let container_id = container
            .id
            .context("docker container inspect response missing Id")?;
        out.insert(
            pod_name,
            PodBackendInfo {
                status,
                container_id,
            },
        );
    }
    Ok(out)
}

fn k8s_list_by_repo(
    backend: &K8sBackend,
    repo_path: &Path,
) -> Result<HashMap<String, PodBackendInfo>> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, ListParams};

    let hash = crate::k8s::repo_path_hash(repo_path);
    let selector = format!("{LABEL_K8S_REPO_HASH}={hash}");
    let pods: Api<Pod> =
        Api::namespaced(backend.client.client().clone(), backend.client.namespace());
    let list = block_on(pods.list(&ListParams::default().labels(&selector)))
        .context("listing k8s pods by repo-hash label")?;
    let mut out = HashMap::new();
    for pod in list.items {
        let labels = pod.metadata.labels.unwrap_or_default();
        let pod_name = match labels.get(LABEL_K8S_POD_NAME) {
            Some(n) => n.clone(),
            None => continue,
        };
        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown");
        let status = match phase {
            "Running" => PodStatus::Running,
            "Pending" => PodStatus::Stopped,
            "Failed" | "Succeeded" => PodStatus::Gone,
            _ => PodStatus::Disconnected,
        };
        let container_id = pod.metadata.name.unwrap_or_default();
        out.insert(
            pod_name,
            PodBackendInfo {
                status,
                container_id,
            },
        );
    }
    Ok(out)
}

/// Map the kubelet's exec status object to an exit code.
///
/// The kubelet uses `reason` to signal success/failure: null or
/// "Completed"/"ExitCode" means exit 0; "NonZeroExitCode" carries
/// the actual code inside `details.causes[].message` keyed by
/// `reason: "ExitCode"`.  Anything else (e.g. a transport error) is
/// reported as a fatal error rather than a synthetic exit code.
fn parse_k8s_exit_code(
    status: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Status,
) -> Result<i32> {
    match status.reason.as_deref() {
        None | Some("") | Some("Completed") | Some("ExitCode") => Ok(0),
        Some("NonZeroExitCode") => {
            let empty = Vec::new();
            let causes = status
                .details
                .as_ref()
                .and_then(|d| d.causes.as_ref())
                .unwrap_or(&empty);
            let code = causes
                .iter()
                .find(|c| c.reason.as_deref() == Some("ExitCode"))
                .and_then(|c| c.message.as_deref())
                .and_then(|m| m.parse::<i32>().ok())
                .unwrap_or(1);
            Ok(code)
        }
        Some(other) => {
            let msg = status.message.as_deref().unwrap_or("");
            Err(anyhow::anyhow!("k8s exec failed ({other}): {msg}"))
        }
    }
}
