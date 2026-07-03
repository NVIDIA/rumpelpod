// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration types for rumpelpod settings.
//!
//! This module provides:
//! - JsonConfig for `.rumpelpod.json`
//! - Utility functions for state directory paths
//!
//! Container settings (image, user, workspace, mounts, runArgs, etc.) come from
//! `devcontainer.json`.  The optional `.rumpelpod.json` provides pod-specific
//! settings that have no devcontainer equivalent (host, merge, ...).
//!
//! The file is parsed with json5, which accepts JSON plus comments and
//! trailing commas (mirroring devcontainer.json's dialect).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use url::Url;

/// Container CLI/runtime used for local container operations.
///
/// `Auto` prefers Docker so existing installations keep their current
/// behavior, then falls back to Podman for Docker-free local hosts.
#[derive(Debug, Clone, Copy, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerEngine {
    #[default]
    Auto,
    Docker,
    Podman,
}

impl ContainerEngine {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "docker" => Ok(Self::Docker),
            "podman" => Ok(Self::Podman),
            _ => Err(format!(
                "unknown container engine '{s}': expected auto, docker, or podman"
            )),
        }
    }

    pub fn binary_name(self) -> &'static str {
        match self {
            Self::Auto => {
                panic!("binary_name() called before resolving container engine auto")
            }
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }

    pub fn resolve(self, allow_podman: bool, reason: &str) -> Result<Self> {
        match self {
            Self::Auto => {
                if crate::which("docker").is_some() {
                    return Ok(Self::Docker);
                }
                if allow_podman && crate::which("podman").is_some() {
                    return Ok(Self::Podman);
                }
                if allow_podman {
                    Err(anyhow::anyhow!(
                        "neither docker nor podman was found on PATH for {reason}"
                    ))
                } else {
                    Err(anyhow::anyhow!("docker was not found on PATH for {reason}"))
                }
            }
            Self::Docker => {
                if crate::which("docker").is_some() {
                    Ok(Self::Docker)
                } else {
                    Err(anyhow::anyhow!(
                        "containerEngine is docker, but docker was not found on PATH for {reason}"
                    ))
                }
            }
            Self::Podman => {
                if !allow_podman {
                    return Err(anyhow::anyhow!(
                        "containerEngine is podman, but podman is not supported for {reason}"
                    ));
                }
                if crate::which("podman").is_some() {
                    Ok(Self::Podman)
                } else {
                    Err(anyhow::anyhow!(
                        "containerEngine is podman, but podman was not found on PATH for {reason}"
                    ))
                }
            }
        }
    }
}

/// Where a pod runs.
///
/// Either the local machine, a remote host accessed via SSH, or a Kubernetes
/// cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Host {
    /// The local Docker or Podman engine.
    Localhost {
        #[serde(default)]
        engine: ContainerEngine,
    },
    /// A remote Docker daemon accessed via SSH.
    Ssh {
        /// The SSH destination string (e.g. "user@host" or just "host").
        /// Passed directly to the `ssh` command.
        ssh_destination: String,
        #[serde(default)]
        engine: ContainerEngine,
    },
    /// A Kubernetes cluster accessed via kubeconfig.
    Kubernetes {
        /// The kubeconfig context name.
        context: String,
        /// The Kubernetes namespace (default "default").
        namespace: String,
        /// Registry that pods pull built images from, and that the
        /// build host pushes to.  Required: every Kubernetes launch builds a
        /// prepared image that the cluster must pull, so there is no
        /// useful Kubernetes configuration without one.
        registry: String,
        /// Node selector labels for pod scheduling.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node_selector: Option<BTreeMap<String, String>>,
        /// Tolerations for pod scheduling.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tolerations: Option<Vec<KubernetesToleration>>,
        /// Name of a `docker buildx` builder to use for image builds.
        /// The builder must already exist (created via
        /// `docker buildx create`).  When not set, images are built
        /// with the local Docker daemon and pushed to the registry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        builder: Option<String>,
        /// Local image builder used when `builder` is not set.
        #[serde(default)]
        image_builder: ContainerEngine,
    },
}

impl Host {
    /// Parse a container host specification from CLI or config file.
    ///
    /// - `"localhost"` means the local Docker or Podman engine.
    /// - `"ssh://user@host"` means a remote Docker or Podman engine via SSH.
    ///
    /// Bare hostnames like `"dev"` are rejected, use `"ssh://dev"` instead.
    /// Kubernetes hosts are configured via `--kubernetes-context` / `kubernetes` instead.
    pub fn parse(s: &str) -> Result<Self> {
        if s == "localhost" {
            return Ok(Host::Localhost {
                engine: ContainerEngine::Auto,
            });
        }

        let url = Url::parse(s).with_context(|| {
            if !s.contains("://") {
                format!(
                    "invalid host '{s}'. Use 'localhost' for a local engine \
                     or 'ssh://host' for a remote engine."
                )
            } else {
                format!("invalid URL: {s}")
            }
        })?;

        match url.scheme() {
            "ssh" => {
                let host = url
                    .host_str()
                    .ok_or_else(|| anyhow::anyhow!("URL must have a host: {s}"))?;
                if let Some(port) = url.port() {
                    return Err(anyhow::anyhow!(
                        "SSH host URLs do not support ports: {s}. \
                         Configure `Port {port}` in SSH config and use \
                         `ssh://{host}` or `ssh://user@{host}`."
                    ));
                }
                let username = url.username();

                let ssh_destination = if !username.is_empty() {
                    format!("{username}@{host}")
                } else {
                    host.to_string()
                };

                Ok(Host::Ssh {
                    ssh_destination,
                    engine: ContainerEngine::Auto,
                })
            }
            other => Err(anyhow::anyhow!(
                "unsupported scheme '{other}' in host '{s}'. \
                     Use 'ssh://' for a remote engine, or \
                     '--kubernetes-context' / 'kubernetes' for Kubernetes."
            )),
        }
    }

    /// The SSH destination string (e.g. "user@host" or "host").
    /// Panics if called on a non-SSH host.
    pub fn ssh_destination(&self) -> &str {
        match self {
            Host::Ssh {
                ssh_destination, ..
            } => ssh_destination,
            Host::Localhost { .. } | Host::Kubernetes { .. } => {
                panic!("ssh_destination() called on non-SSH host")
            }
        }
    }

    /// Whether this is a remote host (SSH or Kubernetes).
    pub fn is_remote(&self) -> bool {
        match self {
            Host::Localhost { .. } => false,
            Host::Ssh { .. } | Host::Kubernetes { .. } => true,
        }
    }

    /// Whether this host uses the Docker-compatible container executor.
    pub fn is_docker(&self) -> bool {
        match self {
            Host::Localhost { .. } | Host::Ssh { .. } => true,
            Host::Kubernetes { .. } => false,
        }
    }

    /// The docker host URI for `docker -H`, e.g. `"ssh://user@host"`.
    /// Returns `None` for localhost (use the default socket instead).
    /// Panics if called on a Kubernetes host.
    pub fn docker_host_uri(&self) -> Option<String> {
        match self {
            Host::Localhost { .. } => None,
            Host::Ssh {
                ssh_destination, ..
            } => Some(format!("ssh://{ssh_destination}")),
            Host::Kubernetes { .. } => {
                panic!("docker_host_uri() called on Kubernetes host")
            }
        }
    }

    pub fn with_container_engine(self, engine: ContainerEngine) -> Self {
        match self {
            Host::Localhost { .. } => Host::Localhost { engine },
            Host::Ssh {
                ssh_destination, ..
            } => Host::Ssh {
                ssh_destination,
                engine,
            },
            Host::Kubernetes {
                context,
                namespace,
                registry,
                node_selector,
                tolerations,
                builder,
                ..
            } => Host::Kubernetes {
                context,
                namespace,
                registry,
                node_selector,
                tolerations,
                builder,
                image_builder: engine,
            },
        }
    }

    pub fn container_engine(&self) -> Option<ContainerEngine> {
        match self {
            Host::Localhost { engine } | Host::Ssh { engine, .. } => Some(*engine),
            Host::Kubernetes { .. } => None,
        }
    }

    pub fn image_builder(&self) -> Option<ContainerEngine> {
        match self {
            Host::Localhost { engine } | Host::Ssh { engine, .. } => Some(*engine),
            Host::Kubernetes { image_builder, .. } => Some(*image_builder),
        }
    }

    /// Resolve `Auto` into a concrete installed engine before a pod is
    /// stored or a build runs.
    pub fn resolve_container_tools(self) -> Result<Self> {
        match self {
            Host::Localhost { engine } => Ok(Host::Localhost {
                engine: engine.resolve(true, "local container executor")?,
            }),
            Host::Ssh {
                ssh_destination,
                engine,
            } => Ok(Host::Ssh {
                ssh_destination,
                engine: engine.resolve(true, "ssh container executor")?,
            }),
            Host::Kubernetes {
                context,
                namespace,
                registry,
                node_selector,
                tolerations,
                builder,
                image_builder,
            } => {
                let reason = if builder.is_some() {
                    "kubernetes.builder, which uses Docker buildx"
                } else {
                    "kubernetes image builder"
                };
                let image_builder = image_builder.resolve(builder.is_none(), reason)?;
                Ok(Host::Kubernetes {
                    context,
                    namespace,
                    registry,
                    node_selector,
                    tolerations,
                    builder,
                    image_builder,
                })
            }
        }
    }
}

impl std::fmt::Display for Host {
    /// Human-readable display for `rumpel list` and error messages.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Host::Localhost { engine } => match engine {
                ContainerEngine::Auto | ContainerEngine::Docker => write!(f, "localhost"),
                ContainerEngine::Podman => write!(f, "localhost (podman)"),
            },
            Host::Ssh {
                ssh_destination,
                engine,
            } => match engine {
                ContainerEngine::Auto | ContainerEngine::Docker => {
                    write!(f, "ssh://{ssh_destination}")
                }
                ContainerEngine::Podman => write!(f, "ssh://{ssh_destination} (podman)"),
            },
            Host::Kubernetes {
                context,
                namespace,
                image_builder,
                ..
            } => {
                if namespace == "default" {
                    match image_builder {
                        ContainerEngine::Auto | ContainerEngine::Docker => {
                            write!(f, "k8s:{context}")
                        }
                        ContainerEngine::Podman => write!(f, "k8s:{context} (podman builder)"),
                    }
                } else {
                    match image_builder {
                        ContainerEngine::Auto | ContainerEngine::Docker => {
                            write!(f, "k8s:{context}/{namespace}")
                        }
                        ContainerEngine::Podman => {
                            write!(f, "k8s:{context}/{namespace} (podman builder)")
                        }
                    }
                }
            }
        }
    }
}

/// A single Kubernetes toleration from `kubernetes.tolerations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KubernetesToleration {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub effect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
}

/// Kubernetes target from the `kubernetes` section in `.rumpelpod.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KubernetesConfig {
    /// The kubeconfig context name.
    pub context: String,
    /// The Kubernetes namespace (default "default").
    pub namespace: Option<String>,
    /// Registry that pods pull built images from, and that the build
    /// host pushes to.  Required: every Kubernetes launch builds a prepared
    /// image that the cluster must pull.
    pub registry: String,
    /// Node selector labels for pod scheduling.
    #[serde(default)]
    pub node_selector: Option<BTreeMap<String, String>>,
    /// Tolerations for pod scheduling.
    #[serde(default)]
    pub tolerations: Option<Vec<KubernetesToleration>>,
    /// Name of a `docker buildx` builder for image builds.
    pub builder: Option<String>,
}

/// Configuration from `.rumpelpod.json`.
///
/// Fields that have equivalents in devcontainer.json (image, user,
/// workspaceFolder, runtime, network) are intentionally omitted -- they
/// should be set in devcontainer.json instead (runtime and network via
/// `runArgs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct JsonConfig {
    #[serde(default)]
    pub claude: ClaudeConfig,

    #[serde(default)]
    pub codex: CodexConfig,

    #[serde(default)]
    pub pi: PiConfig,

    #[serde(default)]
    pub grok: GrokConfig,

    #[serde(default)]
    pub merge: MergeConfig,

    /// Inject a system prompt describing the rumpelpod environment
    /// (devcontainer layout, git remotes, push/fetch behavior) into
    /// each installed agent's system-prompt location (CLAUDE.md,
    /// AGENTS.md, pi's SYSTEM.md).  The description is the same for
    /// every agent, so this is a single pod-level knob rather than a
    /// per-agent one.  Defaults to true.
    #[serde(default = "default_true")]
    pub inject_system_prompt: bool,

    /// Docker host: "localhost" for local or "ssh://user@host" for remote.
    pub host: Option<String>,

    /// Container engine preference for local execution and image builds.
    #[serde(default)]
    pub container_engine: Option<ContainerEngine>,

    /// Kubernetes target. Mutually exclusive with `host`.
    pub kubernetes: Option<KubernetesConfig>,
}

/// Load `.rumpelpod.json` from the given repo root, if present.
pub fn load_json_config(repo_root: &Path) -> Result<JsonConfig> {
    let config_path = repo_root.join(".rumpelpod.json");
    if config_path.exists() {
        let config_path_display = config_path.display();
        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {config_path_display}"))?;
        let config: JsonConfig = json5::from_str(&contents)
            .with_context(|| format!("failed to parse {config_path_display}"))?;

        if config.host.is_some() && config.kubernetes.is_some() {
            return Err(anyhow::anyhow!(
                "'host' and 'kubernetes' are mutually exclusive in {config_path_display}"
            ));
        }

        Ok(config)
    } else {
        // Parse an empty config so serde field defaults (e.g.
        // inject_system_prompt) apply uniformly whether or not the file
        // exists.
        json5::from_str("{}").context("constructing default config")
    }
}

/// Configuration for `rumpel claude`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClaudeConfig {
    /// Pass --dangerously-skip-permissions to the claude CLI.
    /// The pod provides the sandbox so claude does not need its own.
    /// Defaults to true.
    #[serde(default = "default_true")]
    pub dangerously_skip_permissions: bool,

    /// Use a PermissionRequest hook instead of --dangerously-skip-permissions.
    /// Has no effect when `dangerouslySkipPermissions` is false.
    #[serde(default)]
    pub dangerously_skip_permissions_hook: bool,

    /// Copy per-project session JSONLs from
    /// ~/.claude/projects/<encoded-cwd>/ into the pod so
    /// `claude --resume <uuid>` can pick up sessions started on the
    /// host.  Off (omitted or null) by default because the directory
    /// can grow to hundreds of megabytes on active repos, and startup
    /// blocks on the copy.  Present-but-empty (`{}`) enables the copy
    /// with default behaviour; future fields will tune what gets
    /// copied.
    #[serde(default)]
    pub sessions: Option<SessionsConfig>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            dangerously_skip_permissions: true,
            dangerously_skip_permissions_hook: false,
            sessions: None,
        }
    }
}

/// Tunables for the host-to-pod session copy.  Empty for now -- the
/// container exists so `claude.sessions` can grow fields like a
/// max-age cutoff or async/sync toggle without a wider rename.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionsConfig {}

/// Configuration for `rumpel codex`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CodexConfig {
    /// Pass --dangerously-bypass-approvals-and-sandbox to the codex TUI.
    /// The pod provides the sandbox so codex does not need its own.
    /// Defaults to true.
    #[serde(default = "default_true")]
    pub dangerously_bypass_approvals_and_sandbox: bool,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            dangerously_bypass_approvals_and_sandbox: true,
        }
    }
}

/// Configuration for `rumpel pi`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PiConfig {
    /// Pre-trust the workspace so pi's TUI does not block on the
    /// project-trust prompt (it forces `defaultProjectTrust: "always"`
    /// in the copied settings.json).  The pod provides the sandbox, so
    /// pi does not need its own approval gate.  Defaults to true.
    #[serde(default = "default_true")]
    pub trust_workspace: bool,
}

impl Default for PiConfig {
    fn default() -> Self {
        Self {
            trust_workspace: true,
        }
    }
}

/// Configuration for `rumpel grok`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GrokConfig {
    /// Pass --always-approve to the grok CLI so it auto-approves tool
    /// executions.  The pod provides the sandbox so grok does not need
    /// its own approval prompts.  Defaults to true.
    #[serde(default = "default_true")]
    pub always_approve: bool,
}

impl Default for GrokConfig {
    fn default() -> Self {
        Self {
            always_approve: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Whether and how the merge description file feature behaves.
///
/// In `.rumpelpod.json`, under `"merge"`:
///   "description": "auto"       // use if present, skip if absent (default)
///   "description": "required"   // fail merge if file is missing
///   "description": "off"        // disable entirely
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DescriptionMode {
    /// Use the description file if present, skip silently if absent.
    #[default]
    Auto,
    /// Require the description file; fail the merge if absent.
    Required,
    /// Disable the description file feature entirely.
    Off,
}

impl<'de> Deserialize<'de> for DescriptionMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = DescriptionMode;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#""auto", "required", or "off""#)
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "auto" => Ok(DescriptionMode::Auto),
                    "required" => Ok(DescriptionMode::Required),
                    "off" => Ok(DescriptionMode::Off),
                    _ => Err(E::custom(format!(
                        "unknown description mode '{v}': expected \"auto\", \"required\", or \"off\""
                    ))),
                }
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

/// Merge configuration from the `merge` section in `.rumpelpod.json`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MergeConfig {
    /// Whether the description file feature is enabled, and how strict it is.
    /// Default: "auto".
    #[serde(default)]
    pub description: DescriptionMode,

    /// Path of the description file in the pod branch.
    /// Only meaningful when `description` is not "off".
    /// Default: "DESCRIPTION".
    pub description_file: Option<String>,
}

impl MergeConfig {
    /// Resolved description file path, respecting the mode.
    /// Returns None when the feature is off.
    pub fn description_file_path(&self) -> Option<&str> {
        match self.description {
            DescriptionMode::Off => None,
            _ => Some(self.description_file.as_deref().unwrap_or("DESCRIPTION")),
        }
    }
}

/// Get the state directory for rumpelpod data.
/// Uses $XDG_STATE_HOME/rumpelpod or ~/.local/state/rumpelpod as fallback.
pub fn get_state_dir() -> Result<PathBuf> {
    let state_base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .expect("Could not determine home directory")
                .join(".local/state")
        });

    Ok(state_base.join("rumpelpod"))
}

/// Get the runtime directory for rumpelpod sockets.
/// Uses $XDG_RUNTIME_DIR/rumpelpod or /tmp/rumpelpod-<uid> as fallback.
pub fn get_runtime_dir() -> Result<PathBuf> {
    let runtime_base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Fallback to /tmp/rumpelpod-<uid>
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/rumpelpod-{uid}"))
        });

    Ok(runtime_base.join("rumpelpod"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_localhost() {
        let host = Host::parse("localhost").unwrap();
        assert_eq!(
            host,
            Host::Localhost {
                engine: ContainerEngine::Auto
            }
        );
        assert_eq!(host.to_string(), "localhost");
    }

    #[test]
    fn parse_ssh_url() {
        let host = Host::parse("ssh://dev").unwrap();
        assert_eq!(host.ssh_destination(), "dev");
        assert_eq!(host.to_string(), "ssh://dev");
    }

    #[test]
    fn parse_ssh_url_with_user() {
        let host = Host::parse("ssh://user@dev").unwrap();
        assert_eq!(host.ssh_destination(), "user@dev");
        assert_eq!(host.to_string(), "ssh://user@dev");
    }

    #[test]
    fn parse_ssh_url_with_port_rejected() {
        let err = Host::parse("ssh://user@dev:2222").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SSH config") && msg.contains("Port 2222"),
            "error should point users at SSH config: {msg}"
        );
    }

    #[test]
    fn parse_bare_hostname_rejected() {
        let err = Host::parse("dev").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ssh://"),
            "error should suggest ssh:// prefix: {msg}"
        );
    }

    #[test]
    fn parse_unsupported_scheme_rejected() {
        let err = Host::parse("http://dev").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unsupported scheme"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn parse_kubernetes_scheme_rejected() {
        let err = Host::parse("kubernetes://my-cluster").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("kubernetes-context"),
            "error should suggest --kubernetes-context: {msg}"
        );
    }

    #[test]
    fn docker_host_uri_localhost() {
        assert_eq!(
            Host::Localhost {
                engine: ContainerEngine::Auto
            }
            .docker_host_uri(),
            None
        );
    }

    #[test]
    fn docker_host_uri_ssh() {
        let host = Host::parse("ssh://user@dev").unwrap();
        assert_eq!(host.docker_host_uri(), Some("ssh://user@dev".to_string()));
    }

    #[test]
    fn display_k8s_default_namespace() {
        let host = Host::Kubernetes {
            context: "my-cluster".to_string(),
            namespace: "default".to_string(),
            registry: "registry.example".to_string(),
            node_selector: None,
            tolerations: None,
            builder: None,
            image_builder: ContainerEngine::Auto,
        };
        assert_eq!(host.to_string(), "k8s:my-cluster");
    }

    #[test]
    fn display_k8s_custom_namespace() {
        let host = Host::Kubernetes {
            context: "my-cluster".to_string(),
            namespace: "staging".to_string(),
            registry: "registry.example".to_string(),
            node_selector: None,
            tolerations: None,
            builder: None,
            image_builder: ContainerEngine::Auto,
        };
        assert_eq!(host.to_string(), "k8s:my-cluster/staging");
    }

    #[test]
    fn kubernetes_is_remote() {
        let host = Host::Kubernetes {
            context: "my-cluster".to_string(),
            namespace: "default".to_string(),
            registry: "registry.example".to_string(),
            node_selector: None,
            tolerations: None,
            builder: None,
            image_builder: ContainerEngine::Auto,
        };
        assert!(host.is_remote());
        assert!(!host.is_docker());
    }

    #[test]
    fn serde_json_roundtrip() {
        let hosts = vec![
            Host::Localhost {
                engine: ContainerEngine::Auto,
            },
            Host::Ssh {
                ssh_destination: "user@host".to_string(),
                engine: ContainerEngine::Auto,
            },
            Host::Ssh {
                ssh_destination: "dev".to_string(),
                engine: ContainerEngine::Auto,
            },
            Host::Kubernetes {
                context: "my-cluster".to_string(),
                namespace: "staging".to_string(),
                registry: "registry.example".to_string(),
                node_selector: None,
                tolerations: None,
                builder: None,
                image_builder: ContainerEngine::Auto,
            },
        ];
        for host in hosts {
            let json = serde_json::to_string(&host).unwrap();
            let roundtripped: Host = serde_json::from_str(&json).unwrap();
            assert_eq!(host, roundtripped, "roundtrip failed for {host:?}");
        }
    }

    #[test]
    fn parse_kubernetes_node_selector_and_tolerations() {
        let json_str = indoc::indoc! {r#"
            {
              "kubernetes": {
                "context": "test-cluster",
                "registry": "registry.example",
                "nodeSelector": {
                  "pool": "test"
                },
                "tolerations": [
                  {
                    "key": "pool",
                    "value": "test",
                    "effect": "NoSchedule"
                  }
                ]
              }
            }
        "#};
        let config: JsonConfig = json5::from_str(json_str).unwrap();
        let kubernetes = config.kubernetes.unwrap();
        let ns = kubernetes.node_selector.unwrap();
        assert_eq!(ns.get("pool"), Some(&"test".to_string()));
        let tols = kubernetes.tolerations.unwrap();
        assert_eq!(tols.len(), 1);
        assert_eq!(tols[0].key, "pool");
        assert_eq!(tols[0].value.as_deref(), Some("test"));
        assert_eq!(tols[0].effect, "NoSchedule");
        assert_eq!(tols[0].operator, None);
    }

    #[test]
    fn parse_container_engine_preference() {
        let config: JsonConfig = json5::from_str(indoc::indoc! {r#"
            {
              "containerEngine": "podman"
            }
        "#})
        .unwrap();
        assert_eq!(config.container_engine, Some(ContainerEngine::Podman));
    }

    #[test]
    fn parse_unknown_container_engine_rejected() {
        let err = json5::from_str::<JsonConfig>(indoc::indoc! {r#"
            {
              "containerEngine": "containerd"
            }
        "#})
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown variant") || msg.contains("containerd"),
            "error should mention the invalid engine: {msg}"
        );
    }

    #[test]
    fn parse_allows_comments() {
        let json_str = indoc::indoc! {r#"
            {
              // the cluster our pods live in
              "kubernetes": {
                "context": "test-cluster",
                "registry": "registry.example" /* inline */
              }
            }
        "#};
        let config: JsonConfig = json5::from_str(json_str).unwrap();
        assert_eq!(config.kubernetes.unwrap().context, "test-cluster");
    }

    #[test]
    fn kubernetes_config_requires_registry() {
        let json_str = indoc::indoc! {r#"
            {
              "kubernetes": {
                "context": "test-cluster"
              }
            }
        "#};
        let err = json5::from_str::<JsonConfig>(json_str).unwrap_err();
        assert!(
            err.to_string().contains("registry"),
            "error should mention missing registry: {err}"
        );
    }

    #[test]
    fn kubernetes_host_roundtrip_with_node_selector() {
        let mut ns = BTreeMap::new();
        ns.insert("pool".to_string(), "test".to_string());
        let host = Host::Kubernetes {
            context: "cluster".to_string(),
            namespace: "default".to_string(),
            registry: "registry.example".to_string(),
            node_selector: Some(ns),
            tolerations: Some(vec![KubernetesToleration {
                key: "pool".to_string(),
                value: Some("test".to_string()),
                effect: "NoSchedule".to_string(),
                operator: None,
            }]),
            builder: None,
            image_builder: ContainerEngine::Auto,
        };
        let json = serde_json::to_string(&host).unwrap();
        let roundtripped: Host = serde_json::from_str(&json).unwrap();
        assert_eq!(host, roundtripped);
    }

    #[test]
    fn parse_merge_default() {
        let config: JsonConfig = json5::from_str("{}").unwrap();
        assert_eq!(config.merge.description, DescriptionMode::Auto);
        assert_eq!(config.merge.description_file_path(), Some("DESCRIPTION"));
    }

    #[test]
    fn parse_merge_custom_path() {
        let config: JsonConfig = json5::from_str(indoc::indoc! {r#"
            {
              "merge": {
                "descriptionFile": "MERGE_MSG"
              }
            }
        "#})
        .unwrap();
        assert_eq!(config.merge.description, DescriptionMode::Auto);
        assert_eq!(config.merge.description_file_path(), Some("MERGE_MSG"));
    }

    #[test]
    fn parse_merge_off() {
        let config: JsonConfig = json5::from_str(indoc::indoc! {r#"
            {
              "merge": {
                "description": "off"
              }
            }
        "#})
        .unwrap();
        assert_eq!(config.merge.description, DescriptionMode::Off);
        assert_eq!(config.merge.description_file_path(), None);
    }

    #[test]
    fn parse_merge_required() {
        let config: JsonConfig = json5::from_str(indoc::indoc! {r#"
            {
              "merge": {
                "description": "required"
              }
            }
        "#})
        .unwrap();
        assert_eq!(config.merge.description, DescriptionMode::Required);
        assert_eq!(config.merge.description_file_path(), Some("DESCRIPTION"));
    }

    #[test]
    fn parse_merge_off_ignores_path() {
        let config: JsonConfig = json5::from_str(indoc::indoc! {r#"
            {
              "merge": {
                "description": "off",
                "descriptionFile": "MERGE_MSG"
              }
            }
        "#})
        .unwrap();
        assert_eq!(config.merge.description_file_path(), None);
    }

    #[test]
    fn parse_merge_unknown_mode_rejected() {
        let err = json5::from_str::<JsonConfig>(indoc::indoc! {r#"
            {
              "merge": {
                "description": "always"
              }
            }
        "#});
        assert!(err.is_err());
    }
}
