// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Backend-agnostic pod launch spec.
//!
//! Fields on `PodSpec` itself translate directly to both docker and
//! kubernetes.  Fields with no cross-backend analogue live on the
//! optional `docker_only` / `k8s_only` buckets; setting the wrong
//! bucket for the backend fails `Executor::launch` rather than being
//! silently dropped.

use std::collections::{BTreeMap, HashMap};

use crate::config::KubernetesToleration;

use super::Hostname;

/// Everything needed to create a pod.  The `Executor` decides how
/// these fields map to docker CLI arguments / kube-rs `Pod`.
pub struct PodSpec {
    pub image: String,
    pub hostname: Hostname,
    /// None: use image's CMD.  Some: override CMD (ENTRYPOINT preserved
    /// in both backends -- docker's `cmd` and k8s's `args` have the
    /// same semantic).
    pub cmd: Option<Vec<String>>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub seccomp_unconfined: bool,
    pub apparmor_unconfined: bool,
    pub resources: Option<Resources>,
    /// Runtime name.  `None` means the backend default.  Translates to
    /// docker's `--runtime` and k8s's `runtimeClassName`.  Podman uses
    /// containers.conf for runtime selection instead of this field.
    pub runtime: Option<String>,
    pub docker_only: DockerOnly,
    pub k8s_only: K8sOnly,
}

pub struct Mount {
    /// Host path for `Bind`, volume name for `Volume`, ignored for
    /// `Tmpfs`.  Ignored on the kubernetes backend (mounts become
    /// emptyDir volumes keyed by `target`).
    pub source: Option<String>,
    pub target: String,
    pub mount_type: MountType,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountType {
    Bind,
    Volume,
    Tmpfs,
}

pub struct Resources {
    /// CPU in docker-style quantity (e.g. "2") or k8s-style string
    /// (e.g. "500m").  Passed through; no validation.
    pub cpu: Option<String>,
    /// Memory in k8s-style quantity (e.g. "4Gi").  Callers that read
    /// docker-style ("4gb") convert before building the spec.
    pub memory: Option<String>,
}

/// Docker-only knobs.  Populating any field rejects on kubernetes.
#[derive(Default)]
pub struct DockerOnly {
    pub init: bool,
    pub devices: Vec<String>,
    pub network: Option<String>,
    /// Raw `--security-opt` pass-through for values not captured by
    /// `seccomp_unconfined` / `apparmor_unconfined`.
    pub security_opt: Vec<String>,
    /// Map from container port to host port.  Exposed ports are
    /// derived by the executor from the keys.
    pub port_bindings: HashMap<u16, u16>,
}

impl DockerOnly {
    pub fn is_empty(&self) -> bool {
        !self.init
            && self.devices.is_empty()
            && self.network.is_none()
            && self.security_opt.is_empty()
            && self.port_bindings.is_empty()
    }
}

/// Kubernetes-only knobs.  Populating any field rejects on docker.
#[derive(Default)]
pub struct K8sOnly {
    pub node_selector: Option<BTreeMap<String, String>>,
    pub tolerations: Option<Vec<KubernetesToleration>>,
}

impl K8sOnly {
    pub fn is_empty(&self) -> bool {
        self.node_selector.is_none() && self.tolerations.is_none()
    }
}

/// Arguments to `Executor::exec` and friends.
pub struct ExecRequest {
    pub cmd: Vec<String>,
    pub workdir: Option<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
}

pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

/// Streaming handles for a long-lived exec session.
///
/// The caller drives stdin/stdout/stderr directly.  Hold `keepalive`
/// for as long as the session should remain live; the session ends
/// once all four halves are dropped.
pub struct ExecStreams {
    pub stdin: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    pub stdout: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    pub stderr: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    /// Opaque backend-specific handle that anchors the exec session's
    /// lifetime.  Kept by the caller; drop order relative to the
    /// stdio handles does not matter.
    pub keepalive: Box<dyn std::any::Any + Send>,
}
