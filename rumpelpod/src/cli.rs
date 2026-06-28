// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum, ValueHint};

use crate::completions::PodNameCompleter;
use crate::config::Host;

/// Validate that a pod name contains only Docker/git-safe characters:
/// ASCII alphanumeric, hyphens, underscores, and dots.
pub(crate) fn validate_pod_name(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("pod name must not be empty".to_string());
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '-' && *c != '_' && *c != '.')
    {
        return Err(format!(
            "invalid character '{}' in pod name, allowed: ASCII letters, digits, hyphens, underscores, dots",
            bad
        ));
    }
    Ok(name.to_string())
}

/// Shared flags for selecting the target host (Docker or Kubernetes).
#[derive(Args, Clone, Debug)]
pub struct HostArgs {
    /// Docker host: "localhost" or "ssh://user@host".
    /// Overrides .rumpelpod.json setting.
    #[arg(long, conflicts_with_all = ["kubernetes_context", "kubernetes_namespace"])]
    pub host: Option<String>,

    /// Kubernetes context name.
    /// Mutually exclusive with --host.  Requires --kubernetes-registry.
    #[arg(long, requires = "kubernetes_registry")]
    pub kubernetes_context: Option<String>,

    /// Kubernetes namespace (default "default").
    /// Requires --kubernetes-context.
    #[arg(long, requires = "kubernetes_context")]
    pub kubernetes_namespace: Option<String>,

    /// Registry to push/pull built images (e.g. ECR, GHCR, Docker Hub).
    /// Required with --kubernetes-context: every Kubernetes launch builds
    /// a prepared image that the cluster must pull from the registry.
    #[arg(long, requires = "kubernetes_context")]
    pub kubernetes_registry: Option<String>,
}

impl HostArgs {
    /// Build a Host from the CLI flags, if any were given.
    pub fn resolve(&self) -> Result<Option<Host>> {
        if let Some(ref ctx) = self.kubernetes_context {
            let namespace = self
                .kubernetes_namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            if self.host.is_some() {
                return Err(anyhow::anyhow!(
                    "--host and --kubernetes-context are mutually exclusive"
                ));
            }
            let registry = self
                .kubernetes_registry
                .clone()
                .expect("clap `requires = kubernetes_registry` ensures this is set");
            Ok(Some(Host::Kubernetes {
                context: ctx.clone(),
                namespace,
                registry,
                node_selector: None,
                tolerations: None,
                builder: None,
            }))
        } else if let Some(ref h) = self.host {
            Ok(Some(Host::parse(h)?))
        } else {
            Ok(None)
        }
    }
}

/// Pod management tool for LLM agents.
///
/// Spawns isolated Docker containers with automatic git synchronization
/// for running untrusted code safely. Each pod gets its own working copy
/// of your repository where changes are isolated from your local
/// machine.
///
/// SETUP:
///   1. Create a .devcontainer/devcontainer.json for container settings
///   2. Run 'rumpel system-install' to start the background daemon
///   3. Use 'rumpel enter <name>' to create and enter pods
///
/// CONFIGURATION:
///   .devcontainer/devcontainer.json (required):
///     image            Docker image to use
///     workspaceFolder  Where the repo is mounted inside the container
///     containerUser    User inside container (default: image's USER)
///     runArgs          Additional Docker arguments (e.g., --runtime)
///
///   See README.md for the full configuration reference.
#[derive(Parser)]
#[command(name = "rumpelpod", bin_name = "rumpel")]
#[command(version = env!("RUMPELPOD_VERSION_INFO"))]
#[command(verbatim_doc_comment)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug)]
pub struct ListCommand {
    /// Refresh live pod state before printing.
    #[arg(long)]
    pub sync: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a command or interactive shell in a pod.
    ///
    /// The working directory inside the pod corresponds to your current
    /// directory relative to the repository root.
    ///
    /// Examples:
    ///   rumpel enter dev              # Interactive shell
    ///   rumpel enter dev -- make      # Run 'make' in pod
    ///   rumpel enter dev -- cargo test
    #[command(verbatim_doc_comment)]
    Enter(EnterCommand),

    /// List all pods for the current repository.
    ///
    /// Shows pod name, status (running/stopped), and creation time.
    #[command(verbatim_doc_comment)]
    List(ListCommand),

    /// Stop one or more pod containers without removing them.
    ///
    /// The containers are stopped but preserved. Use 'rumpel enter' to
    /// restart them.
    #[command(verbatim_doc_comment)]
    Stop(StopCommand),

    /// Delete a pod and its associated container.
    ///
    /// This stops the container if running and removes all pod state.
    /// Any uncommitted changes in the pod will be lost.
    #[command(verbatim_doc_comment)]
    Delete(DeleteCommand),

    /// Remove all stopped, gone, and broken pods for the current
    /// repository.
    ///
    /// Clean pods are deleted without prompting. Pods with unmerged
    /// commits require interactive confirmation or --force.
    #[command(verbatim_doc_comment)]
    Prune(PruneCommand),

    /// Review changes in a pod using git difftool.
    ///
    /// Shows the diff between the pod's primary branch and the merge
    /// base with its upstream branch. This effectively shows all
    /// changes made in the pod since it diverged from the
    /// corresponding branch on your local machine.
    ///
    /// The pod must have been created while your local repository was
    /// on a branch (not in detached HEAD state) for the upstream to be
    /// set.
    ///
    /// Use -- <path>... to restrict the review to specific paths, like
    /// git difftool.
    ///
    /// Examples:
    ///   rumpel review dev               # Review all changes in 'dev' pod
    ///   rumpel review dev -- src/       # Review only changes under src/
    ///   rumpel review dev -- foo bar    # Review only foo and bar
    #[command(verbatim_doc_comment)]
    Review(ReviewCommand),

    /// Merge a pod's branch into the current branch and stop the pod.
    #[command(verbatim_doc_comment, trailing_var_arg = true)]
    Merge(MergeCommand),

    /// Show forwarded ports for a pod.
    #[command(verbatim_doc_comment)]
    Ports(PortsCommand),

    /// Forward an additional container port to the host.
    ///
    /// Adds a one-off forward on top of the devcontainer.json
    /// `forwardPorts` set.
    ///
    /// Examples:
    ///   rumpel forward-port dev:8080            # auto-pick host port near 8080
    ///   rumpel forward-port dev:8080 9000       # bind exactly 9000 locally
    ///   rumpel forward-port --label "API" dev:8080
    #[command(name = "forward-port", verbatim_doc_comment)]
    ForwardPort(ForwardPortCommand),

    /// Recreate a pod.
    ///
    /// Takes a snapshot of dirty files (including untracked files),
    /// destroys the container, and creates a new one with the snapshot
    /// applied.
    #[command(verbatim_doc_comment)]
    Recreate(RecreateCommand),

    /// Spawn a new pod by cloning an existing, running one.
    #[command(verbatim_doc_comment)]
    Fork(ForkCommand),

    /// Manage devcontainer images.
    #[command(subcommand, verbatim_doc_comment)]
    Image(ImageSubcommand),

    /// Copy files between your local machine and a pod container.
    ///
    /// Uses the in-container HTTP server to transfer tar archives, so
    /// no docker or kubectl CLI is needed locally. Exactly one of src
    /// or dest must use POD:PATH syntax to identify the pod.
    ///
    /// Examples:
    ///   rumpel cp dev:/app/output.txt ./output.txt   # Copy from pod to local machine
    ///   rumpel cp ./input.txt dev:/app/input.txt     # Copy from local machine to pod
    ///   rumpel cp -a dev:/app/dir ./dir              # Archive mode (preserve uid/gid)
    #[command(verbatim_doc_comment)]
    Cp(CpCommand),

    /// Launch Claude Code CLI inside a persistent session in a pod.
    ///
    /// On first run, copies Claude Code config files (~/.claude.json,
    /// ~/.claude/settings.json) from your local machine into the
    /// container. Then attaches to or creates a persistent PTY session
    /// running Claude Code.
    ///
    /// Detach with Ctrl-a d to leave Claude Code running in the
    /// background. Re-run the same command to reattach. If Claude Code
    /// exits (e.g. /exit), the session ends and the next invocation
    /// creates a fresh one.
    ///
    /// Examples:
    ///   rumpel claude dev                    # Launch Claude Code in 'dev' pod
    ///   rumpel claude dev -- --model opus    # Pass args to claude CLI
    #[command(verbatim_doc_comment)]
    Claude(ClaudeCommand),

    /// Run OpenAI Codex inside a pod.
    ///
    /// The Codex App Server runs inside the container while the TUI
    /// runs locally.
    ///
    /// Examples:
    ///   rumpel codex dev                       # Launch Codex in 'dev' pod
    ///   rumpel codex dev -- --model o3         # Pass args to codex TUI
    #[command(verbatim_doc_comment)]
    Codex(CodexCommand),

    /// Run the pi coding agent inside a pod.
    ///
    /// Examples:
    ///   rumpel pi dev                          # Launch pi in 'dev' pod
    ///   rumpel pi dev -- --model anthropic/claude-opus-4-7
    #[command(verbatim_doc_comment)]
    Pi(PiCommand),

    /// Launch the xAI Grok CLI inside a persistent session in a pod.
    ///
    /// On first run, copies your local Grok credentials (~/.grok) and
    /// forwards XAI_API_KEY into the container.  Then attaches to or
    /// creates a persistent PTY session running the grok TUI.
    ///
    /// Detach with Ctrl-a d to leave grok running in the background.
    /// Re-run the same command to reattach.  If grok exits, the session
    /// ends and the next invocation creates a fresh one.
    ///
    /// Examples:
    ///   rumpel grok dev                      # Launch grok in 'dev' pod
    ///   rumpel grok dev -- --model grok-4    # Pass args to grok CLI
    #[command(verbatim_doc_comment)]
    Grok(GrokCommand),

    /// Run ssh-add against a pod's ssh-agent.
    ///
    /// An ssh-agent runs on your local machine for each pod. The agent
    /// socket is relayed into the container so processes inside the pod
    /// can use the keys for authentication (e.g. git push over SSH) but
    /// cannot extract the private key material.
    ///
    /// Arguments after the pod name are passed through to `ssh-add`
    /// verbatim, with `SSH_AUTH_SOCK` pointing at the pod's agent.
    ///
    /// Examples:
    ///   rumpel ssh-add dev ~/.ssh/id_ed25519   # Add a key to 'dev' pod
    ///   rumpel ssh-add dev -l                  # List loaded keys
    ///   rumpel ssh-add dev -D                  # Delete all keys
    #[command(name = "ssh-add", verbatim_doc_comment)]
    SshAdd(SshAddCommand),

    /// Generate shell completion scripts.
    ///
    /// Prints a registration script that teaches your shell how to
    /// complete rumpel commands and pod names. Source the output in
    /// your shell configuration:
    ///
    ///   bash:  eval "$(rumpel completions bash)"
    ///   fish:  rumpel completions fish | source
    ///   zsh:   eval "$(rumpel completions zsh)"
    #[command(verbatim_doc_comment)]
    Completions(CompletionsCommand),

    /// Internal git hook handlers (invoked from git hooks inside containers)
    #[command(subcommand, hide = true)]
    GitHook(GitHookSubcommand),

    /// Internal Claude Code hook handlers
    #[command(subcommand, hide = true)]
    ClaudeHook(ClaudeHookSubcommand),

    /// Run the rumpelpod daemon (internal)
    #[command(hide = true)]
    Daemon,

    /// Run the in-pod tunnel server (internal, started by daemon via kubectl exec)
    #[command(hide = true)]
    TunnelServer,

    /// Bridge stdin/stdout to a TCP connection (internal, started by daemon via exec)
    #[command(hide = true)]
    TcpProxy {
        /// Port to connect to on loopback
        #[arg(long)]
        port: u16,
    },

    /// Execute a command with the pod server's resolved environment (internal)
    #[command(hide = true)]
    ContainerExec {
        /// Working directory
        #[arg(long)]
        workdir: Option<std::path::PathBuf>,
        /// Command and arguments to exec (defaults to login shell)
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },

    /// Run the in-container HTTP server (internal, started by daemon)
    #[command(hide = true)]
    ContainerServe {
        /// Shared secret for authenticating to both the in-container
        /// HTTP server and the host git HTTP server.
        #[arg(long)]
        token: String,
        /// Repository path inside the container
        #[arg(long)]
        repo_path: std::path::PathBuf,
        /// Pod name (used for devcontainerId computation)
        #[arg(long)]
        pod_name: String,
        /// Host-side env vars for ${localEnv:...} resolution (KEY=VALUE)
        #[arg(long = "local-env")]
        local_env: Vec<String>,
        /// JSON-encoded `pod::GitSetupParams` (branches, upstream
        /// rewrites, identity).  Absent means skip git setup.
        #[arg(long)]
        git_setup_spec: Option<String>,
        /// Test-only: exports RUMPELPOD_SERVER_PORT and substitutes
        /// `${containerEnv:VAR}` in /write-home-files payloads.  Not
        /// a public contract.
        #[arg(long, hide = true)]
        test_mode: bool,
    },

    /// Set up repo clone and Claude CLI during image build (internal)
    #[command(hide = true)]
    PrepareImage(PrepareImageCommand),

    /// Install the rumpelpod daemon as a system service.
    ///
    /// Uses systemd on Linux and launchd on macOS. The daemon runs in
    /// the background and manages pod containers, handling git
    /// synchronization and container lifecycle. It starts automatically
    /// on login.
    ///
    /// This is required before using other rumpel commands.
    #[command(verbatim_doc_comment)]
    SystemInstall,

    /// Uninstall the rumpelpod daemon.
    ///
    /// Stops the daemon and removes it from the system service manager.
    /// Existing pod containers are not automatically removed.
    #[command(verbatim_doc_comment)]
    SystemUninstall,

    /// Manage the in-cluster rumpelhub companion deployment.
    ///
    /// rumpelhub runs as a long-lived pod in a Kubernetes namespace
    /// and currently exposes a single GET /healthz endpoint.  See
    /// notes/rumpelhub.md.
    #[command(subcommand, hide = true, verbatim_doc_comment)]
    Hub(HubSubcommand),
}

#[derive(Subcommand)]
pub enum HubSubcommand {
    /// Install (or re-apply) the rumpelhub resources in the target namespace.
    #[command(verbatim_doc_comment)]
    Install(HubCommonArgs),

    /// Delete the rumpelhub resources in the target namespace.
    ///
    /// Idempotent: deleting a hub that was never installed is not an
    /// error.  Does not delete the namespace or any rumpelpods.
    #[command(verbatim_doc_comment)]
    Delete(HubCommonArgs),

    /// Fetch the hub's /healthz endpoint via a port-forward.
    #[command(verbatim_doc_comment)]
    Status(HubCommonArgs),

    /// Run the hub HTTP server (internal, runs inside the hub pod).
    #[command(hide = true)]
    Serve,
}

#[derive(Args)]
pub struct HubCommonArgs {
    /// Kubernetes context name (overrides .rumpelpod.json).
    /// Requires --kubernetes-registry.
    #[arg(long, requires = "kubernetes_registry")]
    pub kubernetes_context: Option<String>,

    /// Kubernetes namespace (overrides .rumpelpod.json, default "default").
    #[arg(long)]
    pub kubernetes_namespace: Option<String>,

    /// Registry to push/pull built images (overrides .rumpelpod.json).
    /// Required together with --kubernetes-context.
    #[arg(long, requires = "kubernetes_context")]
    pub kubernetes_registry: Option<String>,
}

#[derive(Args)]
pub struct EnterCommand {
    /// Name for this pod instance (e.g., 'dev', 'test')
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    #[command(flatten)]
    pub host_args: HostArgs,

    /// Create the pod if it doesn't exist
    #[arg(long)]
    pub create: bool,

    /// Command to run inside the pod (default: interactive shell)
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct StopCommand {
    /// Names of pods to stop
    #[arg(required = true, num_args = 1.., value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub names: Vec<String>,

    /// Block until the container is fully stopped
    #[arg(long)]
    pub wait: bool,
}

#[derive(Args)]
pub struct DeleteCommand {
    /// Names of pods to delete
    #[arg(required = true, num_args = 1.., value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub names: Vec<String>,

    /// Block until the container is fully removed
    #[arg(long)]
    pub wait: bool,

    /// Delete even if the pod has unmerged commits
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct PruneCommand {
    /// Delete even if pods have unmerged commits
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct RecreateCommand {
    /// Name of the pod to recreate
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    #[command(flatten)]
    pub host_args: HostArgs,
}

#[derive(Args)]
pub struct ForkCommand {
    /// Name of the existing pod to fork from
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub source: String,

    /// Name for the new pod
    #[arg(value_parser = validate_pod_name)]
    pub new_name: String,

    /// Proceed without prompting if the source pod's claude or codex
    /// session is mid-turn.
    #[arg(long)]
    pub allow_processing: bool,
}

#[derive(Args)]
pub struct PortsCommand {
    /// Name of the pod
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,
}

/// Parsed `<POD>:<CONTAINER_PORT>` argument for `forward-port`.
#[derive(Clone, Debug)]
pub struct PodPortSpec {
    pub pod_name: String,
    pub container_port: u16,
}

fn parse_pod_port_spec(s: &str) -> Result<PodPortSpec, String> {
    let (pod, port) = s
        .rsplit_once(':')
        .ok_or_else(|| "expected POD:PORT".to_string())?;
    if pod.is_empty() {
        return Err("pod name must not be empty".to_string());
    }
    if port.is_empty() {
        return Err("container port must not be empty".to_string());
    }
    let pod_name = validate_pod_name(pod)?;
    let container_port: u16 = port
        .parse()
        .map_err(|e| format!("invalid container port {port:?}: {e}"))?;
    if container_port == 0 {
        return Err("container port must be non-zero".to_string());
    }
    Ok(PodPortSpec {
        pod_name,
        container_port,
    })
}

fn parse_local_port(s: &str) -> Result<u16, String> {
    let port: u16 = s
        .parse()
        .map_err(|e| format!("invalid local port {s:?}: {e}"))?;
    if port == 0 {
        return Err("local port must be non-zero".to_string());
    }
    Ok(port)
}

#[derive(Args)]
pub struct ForwardPortCommand {
    /// Optional label, shown in `rumpel ports`.
    #[arg(long)]
    pub label: Option<String>,

    /// Pod and container port to forward, e.g. `dev:8080`.
    #[arg(value_name = "POD:PORT", value_parser = parse_pod_port_spec)]
    pub target: PodPortSpec,

    /// Optional fixed local port.  Without this, the daemon picks a
    /// free port near the container port (same as `forwardPorts`).
    #[arg(value_name = "LOCAL_PORT", value_parser = parse_local_port)]
    pub local_port: Option<u16>,
}

#[derive(Args)]
pub struct ReviewCommand {
    /// Name of the pod to review
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    /// Skip prompting before opening each file
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,

    /// Restrict review to specific paths (like git difftool -- <path>...)
    #[arg(last = true, value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Args)]
#[command(trailing_var_arg = true)]
pub struct MergeCommand {
    /// Name of the pod to merge
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    /// Use this file from the pod branch as the merge commit message
    #[arg(long, value_name = "PATH", conflicts_with = "no_description_file")]
    pub description_file: Option<String>,

    /// Do not use a description file for the merge commit message
    #[arg(long, conflicts_with = "description_file")]
    pub no_description_file: bool,

    /// Arguments passed through to git merge (e.g. --no-ff, --squash)
    #[arg(allow_hyphen_values = true)]
    pub git_args: Vec<String>,
}

#[derive(Clone, ValueEnum)]
pub enum ClaudeAction {
    /// Refresh authentication credentials in the pod from your local machine
    Reauth,
}

#[derive(Args)]
pub struct ClaudeCommand {
    /// Name for this pod instance (e.g., 'dev', 'test')
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    /// Action to perform (omit for interactive Claude Code session)
    pub action: Option<ClaudeAction>,

    #[command(flatten)]
    pub host_args: HostArgs,

    /// Create the pod without prompting if it doesn't exist
    #[arg(long)]
    pub create: bool,

    /// Disable --dangerously-skip-permissions (which is on by default)
    #[arg(long, conflicts_with = "dangerously_skip_permissions_hook")]
    pub no_dangerously_skip_permissions: bool,

    /// Use a PermissionRequest hook instead of --dangerously-skip-permissions
    #[arg(long, hide = true)]
    pub dangerously_skip_permissions_hook: bool,

    /// Arguments forwarded to `claude` CLI
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct CodexCommand {
    /// Name for this pod instance (e.g., 'dev', 'test')
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    #[command(flatten)]
    pub host_args: HostArgs,

    /// Create the pod without prompting if it doesn't exist
    #[arg(long)]
    pub create: bool,

    /// Disable --dangerously-bypass-approvals-and-sandbox (which is on by default)
    #[arg(long)]
    pub no_dangerously_bypass_approvals_and_sandbox: bool,

    /// Arguments forwarded to `codex` TUI on your local machine (e.g. --model)
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct PiCommand {
    /// Name for this pod instance (e.g., 'dev', 'test')
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    #[command(flatten)]
    pub host_args: HostArgs,

    /// Create the pod without prompting if it doesn't exist
    #[arg(long)]
    pub create: bool,

    /// Arguments forwarded to the `pi` CLI inside the pod (e.g. --model)
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct GrokCommand {
    /// Name for this pod instance (e.g., 'dev', 'test')
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    #[command(flatten)]
    pub host_args: HostArgs,

    /// Create the pod without prompting if it doesn't exist
    #[arg(long)]
    pub create: bool,

    /// Disable --always-approve (which is on by default)
    #[arg(long)]
    pub no_always_approve: bool,

    /// Arguments forwarded to the `grok` CLI
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct CpCommand {
    #[command(flatten)]
    pub host_args: HostArgs,

    /// Archive mode (preserve uid/gid)
    #[arg(short = 'a', long = "archive")]
    pub archive: bool,

    /// Follow symlinks in src path
    #[arg(short = 'L', long = "follow-link")]
    pub follow_link: bool,

    /// Source: either POD:PATH or a local path
    #[arg(value_name = "SRC", value_hint = ValueHint::AnyPath)]
    pub src: String,

    /// Destination: either POD:PATH or a local path
    #[arg(value_name = "DEST", value_hint = ValueHint::AnyPath)]
    pub dest: String,
}

#[derive(Args)]
pub struct PrepareImageCommand {
    /// Where to clone the repo inside the container
    #[arg(long)]
    pub repo_path: PathBuf,

    /// User to chown the repo to
    #[arg(long)]
    pub user: String,

    /// Claude CLI version to install (skip if not provided)
    #[arg(long)]
    pub claude_version: Option<String>,

    /// pi CLI version to install (skip if not provided)
    #[arg(long)]
    pub pi_version: Option<String>,

    /// Install the Codex CLI into the prepared image
    #[arg(long)]
    pub install_codex: bool,

    /// Install the Grok CLI into the prepared image
    #[arg(long)]
    pub install_grok: bool,

    /// Host git remote to configure (NAME=URL, repeatable)
    #[arg(long = "remote")]
    pub remotes: Vec<String>,

    /// Absolute path at which docker or k8s will later mount a volume,
    /// tmpfs, or bind source.  Pre-created and chowned to `--user` so
    /// docker does not synthesize a root-owned target at container
    /// start; without this, a non-root image USER cannot write to
    /// freshly-created volumes.  Repeatable.
    #[arg(long = "mount-target")]
    pub mount_targets: Vec<String>,

    /// Write a rumpelpod environment description for installed agents
    #[arg(long)]
    pub inject_system_prompt: bool,

    /// Include DESCRIPTION file instructions in the system prompt
    #[arg(long)]
    pub description_file: Option<String>,
}

#[derive(Args)]
pub struct CompletionsCommand {
    /// Shell to generate completions for
    #[arg(value_enum)]
    pub shell: ShellArg,
}

#[derive(Clone, ValueEnum)]
pub enum ShellArg {
    Bash,
    Fish,
    Zsh,
}

#[derive(Subcommand)]
pub enum ImageSubcommand {
    /// Build the devcontainer image from its Dockerfile.
    ///
    /// Requires 'build.dockerfile' in devcontainer.json.
    #[command(verbatim_doc_comment)]
    Build(ImageBuildCommand),

    /// Pull the devcontainer image from its registry.
    ///
    /// Requires 'image' in devcontainer.json (not 'build'). Runs
    /// 'docker pull' so you get the latest version of the image.
    #[command(verbatim_doc_comment)]
    Fetch(ImageFetchCommand),
}

#[derive(Args)]
pub struct ImageBuildCommand {
    #[command(flatten)]
    pub host_args: HostArgs,

    /// Disable Docker layer cache (--no-cache)
    #[arg(long)]
    pub no_cache: bool,

    /// Pull the base image before building (--pull)
    #[arg(long)]
    pub pull: bool,
}

#[derive(Args)]
pub struct ImageFetchCommand {
    #[command(flatten)]
    pub host_args: HostArgs,
}

#[derive(Subcommand)]
pub enum GitHookSubcommand {
    /// Handle git reference-transaction hook events (pod repo)
    ReferenceTransaction(ReferenceTransactionCommand),

    /// Handle git pre-receive hook events (host repo)
    HostPreReceive,

    /// Handle git pre-commit hook events: validate the DESCRIPTION file
    PreCommitDescription(PreCommitDescriptionCommand),
}

#[derive(Subcommand)]
pub enum ClaudeHookSubcommand {
    /// Auto-approve permission dialogs
    PermissionRequest,
    /// Report Claude Code session state to the pod server
    NotifyState(NotifyStateArgs),
}

#[derive(Args)]
pub struct NotifyStateArgs {
    /// The session state to report (processing, waiting_for_input, auth_error, stopped)
    pub state: String,
}

#[derive(Args)]
pub struct ReferenceTransactionCommand {
    /// Transaction state: "prepared", "committed", or "aborted"
    pub state: String,
}

#[derive(Args)]
pub struct PreCommitDescriptionCommand {
    /// Path of the DESCRIPTION file, relative to the repo root
    #[arg(long)]
    pub file: String,
}

#[derive(Args)]
#[command(trailing_var_arg = true)]
pub struct SshAddCommand {
    /// Name of the pod
    #[arg(value_parser = validate_pod_name, add = PodNameCompleter::candidates())]
    pub name: String,

    /// Arguments passed through to ssh-add
    #[arg(allow_hyphen_values = true, value_name = "ARGS")]
    pub args: Vec<String>,
}
