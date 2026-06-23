// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The `Executor` itself.
//!
//! Dispatches backend-agnostic pod ops to a docker or kubernetes
//! implementation.  Callers interact only with this type; the
//! backend variant is an internal detail.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use bollard::Docker;

use crate::async_runtime::block_on;
use crate::config::Host;
use crate::daemon::default_docker_socket;
use crate::daemon::host_connection::HostConnection;
use crate::daemon::protocol::PodStatus;
use crate::k8s::K8sClient;

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
    docker: Docker,
    /// Docker CLI target used for interactive TTY attach, where the
    /// native CLI handles terminal wiring more reliably than bollard.
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
                docker: open_docker(socket)?,
                cli_target: DockerCliTarget::UnixSocket(socket.to_path_buf()),
            }),
        })
    }

    /// Connect to a docker daemon through Docker's SSH transport.
    pub fn docker_ssh(ssh_destination: &str) -> Result<Self> {
        let uri = format!("ssh://{ssh_destination}");
        Ok(Self {
            inner: Inner::Docker(DockerBackend {
                docker: open_docker_ssh(&uri)?,
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
    /// `start_container` is synchronous and no wait is needed.
    pub fn launch(&self, id: &PodId, spec: PodSpec) -> Result<()> {
        match &self.inner {
            Inner::Docker(d) => {
                if !spec.k8s_only.is_empty() {
                    anyhow::bail!(
                        "k8s-only spec fields (node_selector/tolerations) set on a docker launch"
                    );
                }
                docker_launch(&d.docker, id, spec)
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
            Inner::Docker(d) => docker_delete(&d.docker, id),
            Inner::Kubernetes(k) => k.client.delete_pod(id.as_str()),
        }
    }

    /// Current pod status.  Returns `Gone` when the pod no longer
    /// exists on the backend.
    pub fn status(&self, id: &PodId) -> Result<PodStatus> {
        match &self.inner {
            Inner::Docker(d) => docker_status(&d.docker, id),
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
            Inner::Docker(d) => docker_exec(&d.docker, id, req).await,
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
            Inner::Docker(d) => docker_exec_detached(&d.docker, id, req),
            Inner::Kubernetes(k) => k8s_exec_detached(k, id, req),
        }
    }

    /// Start a command inside the pod and return split stdin/stdout/stderr
    /// streams.  The session stays live until the returned `ExecStreams`
    /// is dropped.
    pub async fn exec_streaming(&self, id: &PodId, cmd: Vec<String>) -> Result<ExecStreams> {
        match &self.inner {
            Inner::Docker(d) => docker_exec_streaming(&d.docker, id, cmd).await,
            Inner::Kubernetes(k) => k8s_exec_streaming(k, id, cmd).await,
        }
    }

    /// Stop a running pod without removing it.  Errors on
    /// kubernetes, which has no analogue -- the only way to stop
    /// a k8s pod is to delete it.
    pub fn stop(&self, id: &PodId) -> Result<()> {
        match &self.inner {
            Inner::Docker(d) => docker_stop(&d.docker, id),
            Inner::Kubernetes(_) => {
                anyhow::bail!("stop not supported on kubernetes; delete the pod instead")
            }
        }
    }

    /// Start a stopped pod.  Errors on kubernetes for the same
    /// reason as `stop`.
    pub fn start(&self, id: &PodId) -> Result<()> {
        match &self.inner {
            Inner::Docker(d) => docker_start(&d.docker, id),
            Inner::Kubernetes(_) => {
                anyhow::bail!("start not supported on kubernetes; launch a new pod instead")
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
            Inner::Docker(d) => docker_image_present(&d.docker, image),
            Inner::Kubernetes(_) => Ok(true),
        }
    }

    /// Interactive exec that inherits the caller's stdio.
    ///
    /// Shells out to `docker` or `kubectl` rather than going through
    /// bollard/kube-rs: attaching a TTY from a library requires wiring
    /// up PTY handling for stdin/stdout, which the native CLIs do for
    /// us.  Blocks until the remote process exits.
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
            Inner::Docker(d) => docker_list_by_repo(&d.docker, repo_path),
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
    /// `docker`/`kubectl` (e.g. interactive `docker exec`/`kubectl exec`).
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

fn open_docker(socket: &std::path::Path) -> Result<Docker> {
    Docker::connect_with_socket(
        socket.to_string_lossy().as_ref(),
        120,
        bollard::API_DEFAULT_VERSION,
    )
    .context("connecting to docker daemon")
}

fn open_docker_ssh(uri: &str) -> Result<Docker> {
    Docker::connect_with_ssh(uri, 120, bollard::API_DEFAULT_VERSION, None)
        .context("connecting to docker daemon over ssh")
}

fn docker_delete(docker: &Docker, id: &PodId) -> Result<()> {
    use bollard::errors::Error as BollardError;
    use bollard::query_parameters::{RemoveContainerOptions, StopContainerOptions};

    // Containers typically run `sleep infinity` which won't handle
    // SIGTERM gracefully, so SIGKILL immediately (`t: 0`).  Errors
    // from stop are ignored -- the following remove(force=true) will
    // kill the container if it's still running.
    let stop_options = StopContainerOptions {
        t: Some(0),
        ..Default::default()
    };
    let _ = block_on(docker.stop_container(id.as_str(), Some(stop_options)));

    let remove_options = RemoveContainerOptions {
        force: true,
        ..Default::default()
    };
    match block_on(docker.remove_container(id.as_str(), Some(remove_options))) {
        Ok(()) => Ok(()),
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("docker rm failed: {e}")),
    }
}

fn docker_status(docker: &Docker, id: &PodId) -> Result<PodStatus> {
    use bollard::errors::Error as BollardError;

    match block_on(docker.inspect_container(id.as_str(), None)) {
        Ok(response) => {
            let state = response.state.context("missing container state")?;
            let running = state.running.unwrap_or(false);
            Ok(if running {
                PodStatus::Running
            } else {
                PodStatus::Stopped
            })
        }
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(PodStatus::Gone),
        Err(e) => Err(anyhow::anyhow!("docker inspect failed: {e}")),
    }
}

async fn docker_exec(docker: &Docker, id: &PodId, req: ExecRequest) -> Result<ExecOutput> {
    use bollard::container::LogOutput;
    use bollard::exec::StartExecResults;
    use bollard::models::ExecConfig;
    use tokio::io::AsyncWriteExt;
    use tokio_stream::StreamExt;

    let env: Vec<String> = req.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let config = ExecConfig {
        attach_stdin: Some(req.stdin.is_some()),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        cmd: Some(req.cmd),
        working_dir: req.workdir,
        env: if env.is_empty() { None } else { Some(env) },
        ..Default::default()
    };

    let exec = docker
        .create_exec(id.as_str(), config)
        .await
        .context("creating exec")?;

    let start_result = docker
        .start_exec(&exec.id, None)
        .await
        .context("starting exec")?;
    let (stdout, stderr) = match start_result {
        StartExecResults::Attached { mut output, input } => {
            if let Some(data) = req.stdin.as_ref() {
                let mut input = input;
                input.write_all(data).await.context("writing exec stdin")?;
                input.shutdown().await.context("closing exec stdin")?;
            }
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            while let Some(chunk) = output.next().await {
                match chunk.context("reading exec output")? {
                    LogOutput::StdOut { message } => stdout.extend_from_slice(&message),
                    LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
                    LogOutput::Console { message } => stdout.extend_from_slice(&message),
                    LogOutput::StdIn { .. } => {}
                }
            }
            (stdout, stderr)
        }
        StartExecResults::Detached => (Vec::new(), Vec::new()),
    };

    let inspect = docker
        .inspect_exec(&exec.id)
        .await
        .context("inspecting exec")?;
    let exit_code = inspect.exit_code.unwrap_or(0) as i32;

    Ok(ExecOutput {
        stdout,
        stderr,
        exit_code,
    })
}

async fn k8s_exec(backend: &K8sBackend, id: &PodId, req: ExecRequest) -> Result<ExecOutput> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, AttachParams};
    use tokio::io::AsyncReadExt;

    if req.workdir.is_some() {
        // kube-rs exec has no workdir parameter.  Callers that need it
        // today wrap the command in `sh -c "cd … && …"`; spell that
        // out rather than silently ignoring the field.
        anyhow::bail!(
            "executor::exec workdir is not supported on kubernetes; \
             wrap the command in `sh -c 'cd … && …'` instead"
        );
    }
    if !req.env.is_empty() {
        anyhow::bail!(
            "executor::exec env is not supported on kubernetes; \
             use `env VAR=val … cmd` or the pod's baked env instead"
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
    docker: &Docker,
    id: &PodId,
    cmd: Vec<String>,
) -> Result<ExecStreams> {
    use bollard::container::LogOutput;
    use bollard::exec::StartExecResults;
    use bollard::models::ExecConfig;
    use tokio::io::AsyncWriteExt;
    use tokio_stream::StreamExt;

    let config = ExecConfig {
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        cmd: Some(cmd),
        ..Default::default()
    };

    let exec = docker
        .create_exec(id.as_str(), config)
        .await
        .context("creating streaming exec")?;
    let (output, input) = match docker
        .start_exec(&exec.id, None)
        .await
        .context("starting streaming exec")?
    {
        StartExecResults::Attached { output, input } => (output, input),
        StartExecResults::Detached => {
            return Err(anyhow::anyhow!("streaming exec started in detached mode"))
        }
    };

    // Docker's exec output is multiplexed: a single stream carries
    // interleaved StdOut/StdErr chunks.  Split it into two independent
    // AsyncReads via duplex pipes so callers can treat stdout and
    // stderr like they would on a normal subprocess.
    let (stdout_tx, stdout_rx) = tokio::io::duplex(64 * 1024);
    let (stderr_tx, stderr_rx) = tokio::io::duplex(64 * 1024);

    let demux_handle = tokio::spawn(async move {
        let mut output = output;
        let mut stdout_tx = stdout_tx;
        let mut stderr_tx = stderr_tx;
        while let Some(chunk) = output.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    log::debug!("exec_streaming demux: output error: {e}");
                    break;
                }
            };
            let res = match chunk {
                LogOutput::StdOut { message } | LogOutput::Console { message } => {
                    stdout_tx.write_all(&message).await
                }
                LogOutput::StdErr { message } => stderr_tx.write_all(&message).await,
                LogOutput::StdIn { .. } => Ok(()),
            };
            if res.is_err() {
                break;
            }
        }
    });

    Ok(ExecStreams {
        stdin: Box::new(input),
        stdout: Box::new(stdout_rx),
        stderr: Box::new(stderr_rx),
        // Holding the JoinHandle is enough: the demux task stays
        // scheduled until bollard's output stream ends (which happens
        // when the remote process exits, typically triggered by the
        // caller dropping stdin).
        keepalive: Box::new(demux_handle),
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

fn docker_launch(docker: &Docker, id: &PodId, spec: PodSpec) -> Result<()> {
    use bollard::models::{
        ContainerCreateBody, HostConfig, Mount as BollardMount, MountType as BollardMountType,
        PortBinding,
    };
    use bollard::query_parameters::{CreateContainerOptions, StartContainerOptions};

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

    let env_list: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();

    let bollard_mounts: Vec<BollardMount> = mounts
        .into_iter()
        .map(|m| {
            let typ = match m.mount_type {
                MountType::Bind => BollardMountType::BIND,
                MountType::Volume => BollardMountType::VOLUME,
                MountType::Tmpfs => BollardMountType::TMPFS,
            };
            BollardMount {
                target: Some(m.target),
                source: m.source,
                typ: Some(typ),
                read_only: Some(m.read_only),
                ..Default::default()
            }
        })
        .collect();

    let devices = if docker_only.devices.is_empty() {
        None
    } else {
        Some(
            docker_only
                .devices
                .iter()
                .map(|d| parse_device_mapping(d))
                .collect(),
        )
    };

    let mut security_opt = docker_only.security_opt.clone();
    // `seccomp_unconfined` / `apparmor_unconfined` are the semantic
    // form.  Docker's API takes raw --security-opt strings, so
    // re-materialize them here for the docker path.  Callers
    // wanting raw opts not captured by the semantic booleans put
    // them on docker_only.security_opt and they flow through.
    if seccomp_unconfined {
        security_opt.push("seccomp=unconfined".into());
    }
    if apparmor_unconfined {
        security_opt.push("apparmor=unconfined".into());
    }

    let port_bindings: HashMap<String, Option<Vec<PortBinding>>> = docker_only
        .port_bindings
        .iter()
        .map(|(&container_port, &host_port)| {
            let key = format!("{container_port}/tcp");
            let binding = PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(host_port.to_string()),
            };
            (key, Some(vec![binding]))
        })
        .collect();

    let cap_add = if cap_add.is_empty() {
        None
    } else {
        Some(cap_add)
    };
    let security_opt = if security_opt.is_empty() {
        None
    } else {
        Some(security_opt)
    };

    let host_config = HostConfig {
        runtime,
        network_mode: Some(docker_only.network.unwrap_or_else(|| "bridge".to_string())),
        privileged: if privileged { Some(true) } else { None },
        init: if docker_only.init { Some(true) } else { None },
        cap_add,
        security_opt,
        devices,
        mounts: if bollard_mounts.is_empty() {
            None
        } else {
            Some(bollard_mounts)
        },
        port_bindings: if port_bindings.is_empty() {
            None
        } else {
            Some(port_bindings)
        },
        ..Default::default()
    };

    let exposed_ports: Option<Vec<String>> = if docker_only.port_bindings.is_empty() {
        None
    } else {
        Some(
            docker_only
                .port_bindings
                .keys()
                .map(|port| format!("{port}/tcp"))
                .collect(),
        )
    };

    let config = ContainerCreateBody {
        image: Some(image),
        hostname: Some(hostname.as_str().to_string()),
        labels: Some(labels.into_iter().collect()),
        env: if env_list.is_empty() {
            None
        } else {
            Some(env_list)
        },
        cmd,
        host_config: Some(host_config),
        exposed_ports,
        ..Default::default()
    };

    let options = CreateContainerOptions {
        name: Some(id.as_str().to_string()),
        ..Default::default()
    };

    let response =
        block_on(docker.create_container(Some(options), config)).context("creating container")?;
    block_on(docker.start_container(&response.id, None::<StartContainerOptions>))
        .context("starting container")?;
    Ok(())
}

fn parse_device_mapping(s: &str) -> bollard::models::DeviceMapping {
    // Formats: host (implies container=host, perms=rwm),
    // host:container, host:container:cgroup_permissions.
    let parts: Vec<&str> = s.split(':').collect();
    let (host, container, perms) = match parts.as_slice() {
        [h] => (*h, *h, "rwm"),
        [h, c] => (*h, *c, "rwm"),
        [h, c, p] => (*h, *c, *p),
        _ => (parts[0], parts[1], parts[2]),
    };
    bollard::models::DeviceMapping {
        path_on_host: Some(host.to_string()),
        path_in_container: Some(container.to_string()),
        cgroup_permissions: Some(perms.to_string()),
    }
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

fn docker_exec_detached(docker: &Docker, id: &PodId, req: ExecRequest) -> Result<()> {
    use bollard::exec::StartExecOptions;
    use bollard::models::ExecConfig;

    let env: Vec<String> = req.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let config = ExecConfig {
        cmd: Some(req.cmd),
        working_dir: req.workdir,
        env: if env.is_empty() { None } else { Some(env) },
        ..Default::default()
    };

    let exec =
        block_on(docker.create_exec(id.as_str(), config)).context("creating detached exec")?;
    block_on(docker.start_exec(
        &exec.id,
        Some(StartExecOptions {
            detach: true,
            ..Default::default()
        }),
    ))
    .context("starting detached exec")?;
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
            "executor::exec_detached does not support workdir, env, or stdin on kubernetes; \
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

fn docker_stop(docker: &Docker, id: &PodId) -> Result<()> {
    use bollard::errors::Error as BollardError;
    use bollard::query_parameters::StopContainerOptions;

    let stop_options = StopContainerOptions {
        t: Some(0),
        ..Default::default()
    };
    match block_on(docker.stop_container(id.as_str(), Some(stop_options))) {
        Ok(()) => Ok(()),
        // 304: already stopped.  404: already gone.  Both are
        // success from the caller's perspective.
        Err(BollardError::DockerResponseServerError {
            status_code: 304, ..
        })
        | Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("docker stop failed: {e}")),
    }
}

fn docker_start(docker: &Docker, id: &PodId) -> Result<()> {
    use bollard::query_parameters::StartContainerOptions;

    block_on(docker.start_container(id.as_str(), None::<StartContainerOptions>))
        .context("starting container")?;
    Ok(())
}

fn docker_exec_interactive(
    backend: &DockerBackend,
    id: &PodId,
    cmd: &[String],
    opts: ExecInteractiveOptions,
) -> Result<std::process::ExitStatus> {
    use std::process::Command;

    // `docker exec` treats `--` as a literal argument, so the command
    // follows the container id directly, unlike kubectl.
    let mut c = Command::new("docker");
    match &backend.cli_target {
        DockerCliTarget::UnixSocket(socket) => {
            let socket = socket.display().to_string();
            let docker_host_arg = format!("unix://{socket}");
            c.args(["-H", &docker_host_arg]);
        }
        DockerCliTarget::Ssh { uri } => {
            c.args(["-H", uri]);
        }
    }
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
    use std::process::Command;

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

fn docker_image_present(docker: &Docker, image: &str) -> Result<bool> {
    use bollard::errors::Error as BollardError;
    match block_on(docker.inspect_image(image)) {
        Ok(_) => Ok(true),
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(false),
        Err(e) => Err(anyhow::anyhow!("docker inspect image failed: {e}")),
    }
}

fn docker_list_by_repo(
    docker: &Docker,
    repo_path: &Path,
) -> Result<HashMap<String, PodBackendInfo>> {
    use bollard::models::ContainerSummaryStateEnum;
    use bollard::query_parameters::ListContainersOptions;

    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("{LABEL_DOCKER_REPO_PATH}={}", repo_path.display())],
    );
    let options = ListContainersOptions {
        all: true,
        filters: Some(filters),
        ..Default::default()
    };
    let containers =
        block_on(docker.list_containers(Some(options))).context("listing containers")?;

    let mut out = HashMap::new();
    for container in containers {
        let labels = container.labels.unwrap_or_default();
        let pod_name = match labels.get(LABEL_DOCKER_POD_NAME) {
            Some(n) => n.clone(),
            None => continue,
        };
        let status = match container.state {
            Some(ContainerSummaryStateEnum::RUNNING) => PodStatus::Running,
            _ => PodStatus::Stopped,
        };
        let container_id = container.id.unwrap_or_default();
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
