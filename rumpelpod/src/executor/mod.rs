// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Backend-agnostic pod execution abstraction.
//!
//! Hides whether pods run on Docker or Kubernetes behind a single
//! `Executor` value.  Callers deal with logical names and a
//! backend-agnostic launch spec; the executor owns the backend
//! translation.
//!
//! The name-mangling helpers live in this module (alongside the
//! `PodId`/`Hostname` validity rules) rather than on `Executor`
//! itself: the executor only needs a valid id to operate on, not to
//! know how it was derived.

mod engine;
mod spec;

pub use engine::{ExecInteractiveOptions, Executor, PodBackendInfo};
pub use spec::{
    DockerOnly, ExecOutput, ExecRequest, ExecStreams, K8sOnly, Mount, MountType, PodSpec, Resources,
};

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

use crate::daemon::protocol::PodName;

// Docker labels.  Applied on launch and filtered on list.  Values are
// full strings (docker labels have no DNS-like restrictions).
pub const LABEL_DOCKER_REPO_PATH: &str = "dev.rumpelpod.repo_path";
pub const LABEL_DOCKER_CONTAINER_REPO_PATH: &str = "dev.rumpelpod.container_repo_path";
pub const LABEL_DOCKER_POD_NAME: &str = "dev.rumpelpod.name";

// Kubernetes labels.  Values must satisfy k8s label value rules
// (<=63 chars, [a-z0-9A-Z._-]), so the repo path is stored as a hash
// rather than inline.
pub const LABEL_K8S_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const LABEL_K8S_MANAGED_BY_VALUE: &str = "rumpelpod";
pub const LABEL_K8S_REPO_HASH: &str = "rumpelpod/repo-hash";
pub const LABEL_K8S_POD_NAME: &str = "rumpelpod/pod-name";
pub const ANNOTATION_K8S_CREATED: &str = "rumpelpod/created";

/// Docker labels for a rumpelpod-managed container.
pub fn docker_pod_labels(
    pod_name: &PodName,
    repo_path: &Path,
    container_repo_path: &Path,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        LABEL_DOCKER_REPO_PATH.to_string(),
        repo_path.display().to_string(),
    );
    labels.insert(
        LABEL_DOCKER_CONTAINER_REPO_PATH.to_string(),
        container_repo_path.display().to_string(),
    );
    labels.insert(LABEL_DOCKER_POD_NAME.to_string(), pod_name.0.clone());
    labels
}

/// Kubernetes labels for a rumpelpod-managed pod.
pub fn k8s_pod_labels(pod_name: &PodName, repo_path: &Path) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        LABEL_K8S_MANAGED_BY.to_string(),
        LABEL_K8S_MANAGED_BY_VALUE.to_string(),
    );
    labels.insert(
        LABEL_K8S_REPO_HASH.to_string(),
        crate::k8s::repo_path_hash(repo_path),
    );
    labels.insert(LABEL_K8S_POD_NAME.to_string(), pod_name.0.clone());
    labels
}

/// Kubernetes annotations for a rumpelpod-managed pod.
pub fn k8s_pod_annotations() -> BTreeMap<String, String> {
    let mut annotations = BTreeMap::new();
    annotations.insert(
        ANNOTATION_K8S_CREATED.to_string(),
        chrono::Utc::now().to_rfc3339(),
    );
    annotations
}

/// DNS-1123 label identifying a pod to its backend.
///
/// Restricted to the intersection of docker's and kubernetes's name
/// rules so one value works on both: lowercase alphanumeric and
/// hyphens, 1-63 chars, first and last characters alphanumeric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodId(String);

impl PodId {
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        validate_dns_1123_label(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PodId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// DNS-1123 label set as the container's hostname.
///
/// Same shape as `PodId` but derived from the pod name alone (no
/// repo-path hash): the container's view of its own hostname is a
/// display concern, not an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hostname(String);

impl Hostname {
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        validate_dns_1123_label(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Hostname {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn validate_dns_1123_label(s: &str) -> Result<()> {
    if s.is_empty() {
        bail!("DNS-1123 label is empty");
    }
    if s.len() > 63 {
        let n = s.len();
        bail!("DNS-1123 label too long ({n} > 63): {s:?}");
    }
    let bytes = s.as_bytes();
    let first = bytes[0];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        bail!("DNS-1123 label must start with [a-z0-9]: {s:?}");
    }
    let last = bytes[bytes.len() - 1];
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        bail!("DNS-1123 label must end with [a-z0-9]: {s:?}");
    }
    for &b in bytes {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if !ok {
            bail!("DNS-1123 label must match [-a-z0-9]*: {s:?}");
        }
    }
    Ok(())
}

/// Derive the `PodId` for a logical pod name on a given host repo path.
///
/// Shape: `rumpel-<basename>-<pod>-<hash8>` where `basename` is the
/// repo directory's basename (sanitized to DNS-1123 and capped at
/// `BASENAME_MAX` chars) and `hash8` is an 8-char sha256 prefix of
/// the full repo path.  The basename is a display hint so pods from
/// different repos are recognizable in `docker ps` / `kubectl get
/// pod`; the hash is what actually disambiguates collisions (two
/// repos with the same basename in different parent dirs).  When
/// nothing survives sanitization the basename segment is omitted.
pub fn pod_id_for(pod: &PodName, repo_path: &Path) -> PodId {
    const PREFIX: &str = "rumpel-";
    // DNS-1123 63-char budget:
    //   rumpel- (7) + [<basename> + -] + <pod> + - + <hash8> (9)
    const BASENAME_MAX: usize = 15;

    let hash = repo_path_hash(repo_path);
    let basename = repo_path
        .file_name()
        .map(|n| sanitize_dns_label_fragment(&n.to_string_lossy(), BASENAME_MAX))
        .unwrap_or_default();
    let basename_segment = if basename.is_empty() {
        String::new()
    } else {
        format!("{basename}-")
    };

    // Remaining chars for the pod segment.
    let max_pod = 63 - PREFIX.len() - basename_segment.len() - 1 - hash.len();
    let pod_segment: String = pod.0.chars().take(max_pod).collect();
    let pod_segment = pod_segment.trim_end_matches('-');

    let s = format!("{PREFIX}{basename_segment}{pod_segment}-{hash}");
    PodId::new(s).expect("derived pod id satisfies DNS-1123 by construction")
}

/// Sanitize arbitrary text into a DNS-1123 label fragment: lowercase
/// ASCII alphanumerics survive unchanged; everything else becomes a
/// single hyphen; leading/trailing hyphens are trimmed; the result
/// is capped at `max` chars.  Returns the empty string when nothing
/// survives -- callers omit the segment in that case.
fn sanitize_dns_label_fragment(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    let mut last_was_hyphen = true; // suppresses leading hyphens too
    for c in s.chars() {
        let mapped = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if last_was_hyphen {
                continue;
            }
            last_was_hyphen = true;
        } else {
            last_was_hyphen = false;
        }
        out.push(mapped);
        if out.len() >= max {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Derive a `Hostname` from a logical pod name.  Trivial because
/// `PodName` is itself a DNS-1123 label; this is just a newtype move.
pub fn hostname_for(pod: &PodName) -> Hostname {
    Hostname::new(pod.0.clone()).expect("PodName is a valid DNS-1123 label")
}

fn repo_path_hash(repo_path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_path.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    hex::encode(&hash[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_id_accepts_valid_labels() {
        assert!(PodId::new("a").is_ok());
        assert!(PodId::new("abc").is_ok());
        assert!(PodId::new("a-b").is_ok());
        assert!(PodId::new("a1-b2-c3").is_ok());
        assert!(PodId::new("0").is_ok());
        assert!(PodId::new("a".repeat(63)).is_ok());
    }

    #[test]
    fn pod_id_rejects_invalid_labels() {
        assert!(PodId::new("").is_err());
        assert!(PodId::new("-abc").is_err());
        assert!(PodId::new("abc-").is_err());
        assert!(PodId::new("AbC").is_err());
        assert!(PodId::new("a_b").is_err());
        assert!(PodId::new("a.b").is_err());
        assert!(PodId::new("a b").is_err());
        assert!(PodId::new("a".repeat(64)).is_err());
    }

    #[test]
    fn pod_id_for_is_stable_and_within_limits() {
        let pod = PodName("mypod".to_string());
        let id = pod_id_for(&pod, Path::new("/home/me/repo"));
        assert!(id.as_str().starts_with("rumpel-repo-mypod-"));
        assert!(id.as_str().len() <= 63);
        // Deterministic.
        let id2 = pod_id_for(&pod, Path::new("/home/me/repo"));
        assert_eq!(id, id2);
        // Different repo path, different id even when basename matches.
        let id3 = pod_id_for(&pod, Path::new("/elsewhere/repo"));
        assert_ne!(id, id3);
    }

    #[test]
    fn pod_id_for_sanitizes_and_caps_basename() {
        let pod = PodName("p".to_string());
        // Non-alphanumerics map to a single hyphen; uppercase is
        // lowered.  The 15-char basename cap truncates "project" to
        // "projec".
        let id = pod_id_for(&pod, Path::new("/tmp/My.Weird_Project!"));
        assert!(
            id.as_str().starts_with("rumpel-my-weird-projec-p-"),
            "unexpected id: {}",
            id.as_str()
        );
        // Over-long pure-alphanumeric basename capped at 15 chars.
        let id = pod_id_for(&pod, Path::new("/tmp/aaaaaaaaaaaaaaaaaaaaaa"));
        assert!(id.as_str().starts_with("rumpel-aaaaaaaaaaaaaaa-p-"));
    }

    #[test]
    fn pod_id_for_omits_basename_when_unrepresentable() {
        let pod = PodName("p".to_string());
        // Root path: no file_name, so no basename segment.
        let id = pod_id_for(&pod, Path::new("/"));
        assert!(id.as_str().starts_with("rumpel-p-"));
        // Basename made entirely of separators sanitizes to empty.
        let id = pod_id_for(&pod, Path::new("/tmp/___"));
        assert!(id.as_str().starts_with("rumpel-p-"));
    }

    #[test]
    fn pod_id_for_truncates_long_pod_names() {
        let pod = PodName("a".repeat(100));
        let id = pod_id_for(&pod, Path::new("/tmp/myrepo"));
        assert!(id.as_str().len() <= 63);
        assert!(id.as_str().starts_with("rumpel-myrepo-"));
        // Ends with "-<8 hex chars>".
        let tail = &id.as_str()[id.as_str().len() - 9..];
        assert!(tail.starts_with('-'));
        assert!(tail[1..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hostname_for_copies_pod_name() {
        let pod = PodName("my-pod".to_string());
        assert_eq!(hostname_for(&pod).as_str(), "my-pod");
    }
}
