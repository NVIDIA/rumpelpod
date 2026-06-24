// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use crate::async_runtime::block_on;
use crate::daemon::protocol::PodStatus;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};
use log::{info, trace};

/// Client for Kubernetes operations, scoped to a specific context and namespace.
#[derive(Clone)]
pub struct K8sClient {
    client: Client,
    context: String,
    namespace: String,
}

/// Options for creating a Kubernetes pod beyond the basics (image, labels, etc.).
#[derive(Default)]
pub struct K8sPodOptions {
    pub volumes: Vec<K8sVolumeMount>,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub seccomp_unconfined: bool,
    pub apparmor_unconfined: bool,
    /// Optional hostname override.  None leaves `spec.hostname`
    /// unset, in which case kubernetes defaults it to the pod name.
    pub hostname: Option<String>,
    /// CMD override.  None uses the image's CMD unchanged; Some(v)
    /// sets `args = v` (ENTRYPOINT preserved).
    pub cmd: Option<Vec<String>>,
    pub resource_requests: Option<K8sResourceRequests>,
    pub node_selector: Option<BTreeMap<String, String>>,
    pub tolerations: Option<Vec<crate::config::KubernetesToleration>>,
    pub runtime_class_name: Option<String>,
}

/// A volume mount for a Kubernetes pod (backed by emptyDir).
pub struct K8sVolumeMount {
    /// Volume name, e.g. "vol-0", "vol-1".
    pub name: String,
    /// Mount path inside the container.
    pub mount_path: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
    /// "Memory" for tmpfs-backed emptyDir, None for disk-backed.
    pub medium: Option<String>,
}

/// Resource requests for a Kubernetes pod.
pub struct K8sResourceRequests {
    /// CPU request, e.g. "4" for 4 cores.
    pub cpu: Option<String>,
    /// Memory request, e.g. "4Gi".
    pub memory: Option<String>,
}

/// Compute a short hash of the repo path for use as a label value.
/// Label values must be <= 63 chars and match [a-z0-9A-Z._-].
pub fn repo_path_hash(repo_path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(repo_path.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    hex::encode(&hash[..8])
}

impl K8sClient {
    /// The underlying kube client.  Used by the executor abstraction
    /// to build its own `Api` handles without going through this
    /// type's higher-level methods.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The namespace this client is scoped to.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The kube context this client is scoped to.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// Create a new Kubernetes client for the given context and namespace.
    pub fn new(context: &str, namespace: &str) -> Result<Self> {
        let client = block_on(async {
            let kubeconfig = Kubeconfig::read().context("reading kubeconfig")?;
            let config = Config::from_custom_kubeconfig(
                kubeconfig,
                &KubeConfigOptions {
                    context: Some(context.to_string()),
                    ..Default::default()
                },
            )
            .await
            .context("building kube config from kubeconfig")?;
            Client::try_from(config).context("creating kube client")
        })?;

        Ok(Self {
            client,
            context: context.to_string(),
            namespace: namespace.to_string(),
        })
    }

    /// Create a Kubernetes pod running the given image.
    ///
    /// When `options.override_command` is true, the container args are set to
    /// `["sleep", "infinity"]` (overriding the image CMD but preserving the
    /// ENTRYPOINT).  When false, the image's own CMD is used.
    #[allow(clippy::too_many_arguments)]
    pub fn create_pod(
        &self,
        name: &str,
        image: &str,
        labels: BTreeMap<String, String>,
        mut annotations: BTreeMap<String, String>,
        env: &[(String, String)],
        options: &K8sPodOptions,
    ) -> Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);

        // Build volume mounts and volume definitions from options
        let volume_mounts: Vec<serde_json::Value> = options
            .volumes
            .iter()
            .map(|v| {
                serde_json::json!({
                    "name": v.name,
                    "mountPath": v.mount_path,
                    "readOnly": v.read_only,
                })
            })
            .collect();

        let volumes: Vec<serde_json::Value> = options
            .volumes
            .iter()
            .map(|v| {
                let empty_dir = match &v.medium {
                    Some(medium) => serde_json::json!({ "medium": medium }),
                    None => serde_json::json!({}),
                };
                serde_json::json!({
                    "name": v.name,
                    "emptyDir": empty_dir,
                })
            })
            .collect();

        let mut container = serde_json::json!({
            "name": "main",
            "image": image,
            "env": env.iter().map(|(k, v)| serde_json::json!({
                "name": k,
                "value": v,
            })).collect::<Vec<_>>(),
            "volumeMounts": volume_mounts,
        });

        if let Some(ref args) = options.cmd {
            container["args"] = serde_json::json!(args);
        }

        // Build securityContext.
        // No runAsUser: the ENTRYPOINT must run as the image's USER.
        // kubectl exec enters as that same user; switch_user() is a
        // no-op when image USER == container user (the common case)
        // and drops privileges when the image USER is root.
        let mut sec_ctx = serde_json::Map::new();

        if options.privileged {
            sec_ctx.insert("privileged".to_string(), serde_json::json!(true));
        }

        if !options.cap_add.is_empty() {
            sec_ctx.insert(
                "capabilities".to_string(),
                serde_json::json!({ "add": options.cap_add }),
            );
        }

        if options.seccomp_unconfined {
            sec_ctx.insert(
                "seccompProfile".to_string(),
                serde_json::json!({ "type": "Unconfined" }),
            );
        }

        if !sec_ctx.is_empty() {
            container["securityContext"] = serde_json::Value::Object(sec_ctx);
        }

        // Build resource requests
        if let Some(ref reqs) = options.resource_requests {
            let mut requests = serde_json::Map::new();
            if let Some(ref cpu) = reqs.cpu {
                requests.insert("cpu".to_string(), serde_json::json!(cpu));
            }
            if let Some(ref memory) = reqs.memory {
                requests.insert("memory".to_string(), serde_json::json!(memory));
            }
            if !requests.is_empty() {
                container["resources"] = serde_json::json!({
                    "requests": serde_json::Value::Object(requests),
                });
            }
        }

        // AppArmor unconfined via annotation
        if options.apparmor_unconfined {
            annotations.insert(
                "container.apparmor.security.beta.kubernetes.io/main".to_string(),
                "unconfined".to_string(),
            );
        }

        let mut pod_spec = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": name,
                "labels": labels,
                "annotations": annotations,
            },
            "spec": {
                "containers": [container],
                "volumes": volumes,
                "restartPolicy": "Never",
            }
        });

        // Without spec.hostname kubernetes defaults the pod's
        // hostname to the mangled pod name (e.g. "rp-mypod-abc123"),
        // which shows up as $HOSTNAME inside the container.  Prefer
        // the caller-provided label so $HOSTNAME matches docker,
        // where we already set it.
        if let Some(ref hostname) = options.hostname {
            pod_spec["spec"]["hostname"] = serde_json::json!(hostname);
        }

        if let Some(ref runtime_class_name) = options.runtime_class_name {
            pod_spec["spec"]["runtimeClassName"] = serde_json::json!(runtime_class_name);
            if runtime_class_name == "sysbox-runc" {
                // sysbox-runc requires the pod to run in a Kubernetes-
                // managed user namespace (KEP-127).  Without hostUsers:
                // false, containerd 2.x will not set up the userns and
                // sysbox fails to mount sysfs into the sandbox.
                info!(
                    "setting hostUsers: false for sysbox-runc \
                     (required for user-namespace isolation)"
                );
                pod_spec["spec"]["hostUsers"] = serde_json::json!(false);
            }
        }

        if let Some(ref ns) = options.node_selector {
            pod_spec["spec"]["nodeSelector"] = serde_json::json!(ns);
        }

        if let Some(ref tolerations) = options.tolerations {
            let vals: Vec<serde_json::Value> = tolerations
                .iter()
                .map(|t| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("key".to_string(), serde_json::json!(t.key));
                    obj.insert("effect".to_string(), serde_json::json!(t.effect));
                    if let Some(ref v) = t.value {
                        obj.insert("value".to_string(), serde_json::json!(v));
                    }
                    obj.insert(
                        "operator".to_string(),
                        serde_json::json!(t.operator.as_deref().unwrap_or("Equal")),
                    );
                    serde_json::Value::Object(obj)
                })
                .collect();
            pod_spec["spec"]["tolerations"] = serde_json::json!(vals);
        }

        let pod: Pod = serde_json::from_value(pod_spec).context("serializing pod spec")?;

        block_on(async {
            pods.create(&PostParams::default(), &pod)
                .await
                .context("creating pod")?;
            Ok::<_, anyhow::Error>(())
        })?;

        info!("Created k8s pod '{name}'");
        Ok(())
    }

    /// Wait for a pod to reach the Running phase.
    ///
    /// Polls indefinitely (the user can Ctrl-C).
    pub fn wait_running(&self, name: &str) -> Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut last_log = std::time::Instant::now();

        loop {
            let pod = block_on(pods.get(name)).with_context(|| format!("getting pod '{name}'"))?;

            let phase = pod
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("Unknown");

            match phase {
                "Running" => return Ok(()),
                "Pending" => {
                    // Check for container errors (e.g. ImagePullBackOff)
                    if let Some(ref status) = pod.status {
                        if let Some(ref statuses) = status.container_statuses {
                            for cs in statuses {
                                if let Some(waiting) =
                                    cs.state.as_ref().and_then(|s| s.waiting.as_ref())
                                {
                                    let reason = waiting.reason.as_deref().unwrap_or("Unknown");
                                    if reason.contains("Err") || reason.contains("BackOff") {
                                        let msg = waiting.message.as_deref().unwrap_or("");
                                        return Err(anyhow::anyhow!(
                                            "pod '{}' failed to start: {} {}",
                                            name,
                                            reason,
                                            msg
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                "Failed" | "Succeeded" => {
                    return Err(anyhow::anyhow!(
                        "pod '{}' is in terminal phase '{}'",
                        name,
                        phase
                    ));
                }
                _ => {}
            }

            if last_log.elapsed() >= std::time::Duration::from_secs(10) {
                info!("Still waiting for pod '{name}' to start (status: {phase})");
                last_log = std::time::Instant::now();
            }

            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    /// Delete a pod. Does not wait for termination.  Idempotent: if the
    /// pod is already gone the API call returns NotFound, which we
    /// translate into success so callers don't have to special-case
    /// out-of-band deletes (e.g. a user invoking `kubectl delete` or
    /// the cluster reaping the pod).
    pub fn delete_pod(&self, name: &str) -> Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let result = block_on(pods.delete(
            name,
            &DeleteParams {
                grace_period_seconds: Some(0),
                ..Default::default()
            },
        ));
        match result {
            Ok(_) => {
                info!("Deleted k8s pod '{name}'");
                Ok(())
            }
            Err(kube::Error::Api(s)) if s.is_not_found() => {
                info!("k8s pod '{name}' was already gone");
                Ok(())
            }
            Err(e) => Err(anyhow::Error::new(e).context("deleting pod")),
        }
    }

    /// Get the status of a pod, mapped to PodStatus.
    pub fn get_pod_status(&self, name: &str) -> Result<PodStatus> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod = match block_on(pods.get_opt(name)).context("getting pod status")? {
            Some(pod) => pod,
            None => return Ok(PodStatus::Gone),
        };

        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Unknown");

        Ok(match phase {
            "Running" => PodStatus::Running,
            "Pending" => PodStatus::Stopped,
            "Failed" | "Succeeded" => PodStatus::Gone,
            _ => PodStatus::Disconnected,
        })
    }

    /// Set up port forwarding from a local port to a pod port.
    ///
    /// Uses a kubectl subprocess instead of kube-rs portforward because
    /// the kube-rs WebSocket connection drops on high-latency links
    /// (e.g. devcontainer -> remote k8s over the internet).
    pub fn port_forward(&self, name: &str, remote_port: u16) -> Result<PortForwardHandle> {
        use std::process::{Command, Stdio};

        // Bind an ephemeral port, then release it for kubectl to use.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("binding ephemeral port for k8s port-forward")?;
        let local_port = listener.local_addr()?.port();
        drop(listener);

        let mut child = Command::new("kubectl")
            .args(["--context", &self.context])
            .args(["-n", &self.namespace])
            .args(["port-forward", name, &format!("{local_port}:{remote_port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting kubectl port-forward")?;

        // Wait for kubectl to start accepting connections.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if std::net::TcpStream::connect(format!("127.0.0.1:{local_port}")).is_ok() {
                break;
            }
            if std::time::Instant::now() > deadline {
                // Check if kubectl is still alive
                let status = child.try_wait().context("checking kubectl status")?;
                let detail = match status {
                    Some(exit) => {
                        let mut stderr_str = String::new();
                        if let Some(mut stderr) = child.stderr.take() {
                            use std::io::Read;
                            let _ = stderr.read_to_string(&mut stderr_str);
                        }
                        format!("kubectl exited with {exit}: {stderr_str}")
                    }
                    None => "kubectl still running but port not accepting connections".to_string(),
                };
                return Err(anyhow::anyhow!(
                    "kubectl port-forward to {name}:{remote_port} did not become ready: {detail}"
                ));
            }
            // Check early exit before sleeping
            if let Ok(Some(exit)) = child.try_wait() {
                let mut stderr_str = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut stderr_str);
                }
                return Err(anyhow::anyhow!(
                    "kubectl port-forward to {name}:{remote_port} exited with {exit}: {stderr_str}"
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        trace!(
            "Port forward established: 127.0.0.1:{} -> {}:{}",
            local_port,
            name,
            remote_port
        );

        Ok(PortForwardHandle {
            local_port,
            _child: child,
        })
    }
}

/// Handle for an active port-forward. Dropping this cancels the forward.
pub struct PortForwardHandle {
    pub local_port: u16,
    _child: std::process::Child,
}

impl Drop for PortForwardHandle {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}

/// Convert a devcontainer-style memory string (e.g. "4gb", "512mb") to
/// Kubernetes resource quantity format (e.g. "4Gi", "512Mi").
///
/// Returns None if the string cannot be parsed.
pub fn convert_memory_to_k8s(s: &str) -> Option<String> {
    let s = s.trim().to_lowercase();

    let suffixes: &[(&str, &str)] = &[("tb", "Ti"), ("gb", "Gi"), ("mb", "Mi"), ("kb", "Ki")];

    for (suffix, k8s_suffix) in suffixes {
        if let Some(num_str) = s.strip_suffix(suffix) {
            let num: u64 = num_str.trim().parse().ok()?;
            return Some(format!("{num}{k8s_suffix}"));
        }
    }

    // No suffix -- treat as raw bytes
    let bytes: u64 = s.parse().ok()?;
    Some(format!("{bytes}"))
}
