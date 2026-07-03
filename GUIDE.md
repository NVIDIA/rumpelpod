# Rumpelpod Guide

Rumpelpod runs LLM agents in isolated devcontainers.
Each pod is a container with its own checkout of the repository, synchronized back to your local machine via git.
Pods can run on the local Docker or Podman engine, on a remote Docker machine over SSH, or as Kubernetes pods.

## Table of contents

- [Quick start](#quick-start)
- [Container configuration](#container-configuration)
- [Remote hosts and security](#remote-hosts-and-security)
- [Advanced usage](#advanced-usage)
- [Installation](#installation)
- [.rumpelpod.json reference](#rumpelpodjson-reference)

## Quick start

Yolo install (see [Installation](#installation) for details):

```sh
curl -fsSL https://raw.githubusercontent.com/nvidia/rumpelpod/main/install.sh | sh
rumpel system-install
```

From inside one of your repositories, run:

```sh
rumpel claude my-pod
```

This launches a persistent Claude Code session in auto-accept mode inside an isolated container called a pod.
The pod gets its own checkout of the repository and a copy of your Claude credentials from your local machine.
Its container image is based on existing [devcontainer configuration](#container-configuration) or a default image.
The first invocation builds the image, which can take a while; subsequent pods reuse it and start much faster.
`rumpel codex` does the same for OpenAI Codex, and `rumpel pi` for the [pi](https://pi.dev) coding agent.

Have the agent make a change and commit it.
Once the commit lands, rumpelpod automatically pushes it to your local repository, where it appears as `rumpelpod/my-pod`.
Detach from the session with `Ctrl-a d` and inspect the changes locally:

```sh
rumpel review my-pod
```

This opens your configured `git difftool` on each changed file, comparing the pod branch against the point where it diverged from your working branch.

When you are satisfied, merge the pod branch into your current branch:

```sh
rumpel merge my-pod
```

If the pod maintained a `DESCRIPTION` file (which rumpelpod's injected system prompt asks the agent to do), its contents become the merge commit message and the file is removed from the tree.

For cases where you need a shell inside the container, use `rumpel enter`:

```sh
rumpel enter my-pod
```

Other useful commands: `rumpel list` shows all pods for the current repository, `rumpel delete` removes a pod and its container, and `rumpel prune` deletes all stopped pods for the current repository.

## Container configuration

Rumpelpod consumes `devcontainer.json` following the [Dev Container spec][spec].
Several of the fields the spec defines overlap with features already available in a Dockerfile, in which case the Dockerfile is usually the more natural place to set them.
The sections below cover the fields that remain useful regardless, followed by rumpelpod's deviations from the spec.

[spec]: https://containers.dev/implementors/json_reference/

The first decision is which container image to use.
Either set `image` to a published image, or set `build.dockerfile` to build one from a Dockerfile in the repository.
When you use `build`, keep both `devcontainer.json` and the Dockerfile inside `.devcontainer/` rather than at the repo root, so that the build context stays confined to that directory.
With them at the root, every edit anywhere in the repo would invalidate the image cache and force a rebuild.

```jsonc
// .devcontainer/devcontainer.json
{
  "build": { "dockerfile": "Dockerfile" }
}
```

```dockerfile
# .devcontainer/Dockerfile
FROM debian:testing

RUN apt-get update && apt-get install --yes git npm
```

### Per-developer customization

If the container image should be customizable by each developer, `${localEnv:VAR}` substitutions can be used.
The syntax works in any string field of `devcontainer.json` and substitutes a value from the local environment, with an optional default after a second colon.
For example, to let each developer use a personal base image with their own devtools pre-installed:

```jsonc
// .devcontainer/devcontainer.json
{
  "build": {
    "dockerfile": "Dockerfile",
    "args": {
      "BASE_IMAGE": "${localEnv:BASE_IMAGE:debian}"
    }
  }
}
```

```dockerfile
# .devcontainer/Dockerfile
ARG BASE_IMAGE=debian
FROM $BASE_IMAGE
```

### Secrets and credentials

Secret tokens should not be baked into the container image, since the image may be pushed to a registry.
There are several ways to inject secrets at runtime instead.

For environment variables such as API keys, use `containerEnv` with `localEnv` substitutions.
Developers set the variable in the shell where they invoke `rumpel`:

```jsonc
// .devcontainer/devcontainer.json
{ "containerEnv": { "API_TOKEN": "${localEnv:API_TOKEN}" } }
```

Alternatively, store secrets in a gitignored `.env` file and reference it through `runArgs`:

```jsonc
// .devcontainer/devcontainer.json
{ "runArgs": ["--env-file", ".env"] }
```

For secret files such as certificates, use a bind mount pointing at a gitignored directory:

```jsonc
// .devcontainer/devcontainer.json
{
  "mounts": [{
    "type": "bind",
    "source": "${localWorkspaceFolder}/secrets",
    "target": "${containerWorkspaceFolder}/secrets"
  }]
}
```

For SSH keys, use agent forwarding rather than copying key files into the pod.
See [SSH key forwarding](#ssh-key-forwarding).

### Warm build caches

Rumpelpod derives a per-repo image on top of the configured base image, with a checkout of the repository baked in.
On container start, only commits that did not exist when the image was built need to be pulled.
However, the first build inside a fresh pod still runs from zero because there are no build caches.

The clone and the build can instead be performed inside the Dockerfile itself, as a later layer.
The resulting image ships with populated build caches, so new pods start warm.
Because image layers are shared, multiple pods on the same machine reuse one read-only copy of the cache rather than each carrying its own.

For a Cargo project, the Dockerfile might end with a clone and a test build so that compiled artifacts land in the image's `target/` directory:

```dockerfile
# .devcontainer/Dockerfile
RUN git clone https://github.com/your-org/your-project /workspaces/your-project
WORKDIR /workspaces/your-project
RUN cargo test --no-run
```

Set `workspaceFolder` in `devcontainer.json` to ensure rumpelpod uses the same path as the baked checkout:

```jsonc
// .devcontainer/devcontainer.json
{ "workspaceFolder": "/workspaces/your-project" }
```

### Devcontainer deviations

A handful of fields are either architecturally incompatible with rumpelpod or deliberately left out.
Rumpelpod ignores them with a warning when they appear.

- `features` and `overrideFeatureInstallOrder`: use a Dockerfile to install the equivalent packages.
- `dockerComposeFile`, `service`, `runServices`: rumpelpod operates on single containers.
- `workspaceMount`: rumpelpod syncs the workspace via git rather than bind-mounting it.
- `appPort`: use `forwardPorts` instead, so that rumpelpod's port tracking can remap across pods.
- `shutdownAction`: containers stay running between sessions and are only removed explicitly with `rumpel delete`.
- `initializeCommand` and `postAttachCommand` are ignored.
  The remaining lifecycle commands run once per container start.
- `userEnvProbe` is supported but behaves differently: the probe runs once when the pod is first created and the result is cached for the container's lifetime.
  Changes to shell init files do not take effect until the pod is recreated.
- Bind mounts work fully only when containers run locally.
  On remote machines and Kubernetes, the source is copied into the pod once at creation and never synchronized back.
  Use `rumpel cp` to move files across the boundary manually.

One further consequence of the git-based workspace is that untracked and gitignored files from the local checkout are not present in the container.
Use bind mounts or environment variables to inject them (see [Secrets and credentials](#secrets-and-credentials)).

## Remote hosts and security

Pods run on the local container engine by default, but the recommended setup is a remote machine.
Running pods remotely provides physical separation between the development machine and the agent's environment.
Agents continue running when you disconnect, so long-running tasks do not depend on a local session staying open.
A remote machine can also be sized for the workload, which matters when multiple agents compile and run tests concurrently.

There are two ways to run pods remotely: on a machine you can SSH into that has Docker installed, or on a Kubernetes cluster.

### SSH with Docker

The remote machine needs Docker installed, and the user you connect as must have permission to talk to the Docker daemon (typically by being in the `docker` group).
Rumpelpod never prompts for a password, so passwordless authentication must already be in place, via an SSH agent or a key configured in `~/.ssh/config`.

Set up the remote machine as a normal OpenSSH target first.
Put host-specific details such as usernames, non-default ports, keepalives, identities, and connection sharing in `~/.ssh/config`:

```sshconfig
Host dev-box
    HostName dev-box.example.com
    User user
    Port 2222

    # Reuse SSH connections to avoid slow reconnects.
    ControlMaster auto
    ControlPath ~/.ssh/sockets/%r@%h-%p
    ControlPersist 10m

    # Fail fast when the remote host or network is no longer reachable.
    ServerAliveInterval 5
    ServerAliveCountMax 2
    TCPKeepAlive yes
    ConnectTimeout 10
```

Create the control socket directory before using that `ControlPath`:

```sh
mkdir -p ~/.ssh/sockets
chmod 700 ~/.ssh/sockets
```

Use the configured host name in `.rumpelpod.json`:

```jsonc
// .rumpelpod.json
{ "host": "ssh://dev-box" }
```

The same host can be selected per invocation:

```sh
rumpel claude my-pod --host ssh://dev-box
```

### Kubernetes

Pods can also run as Kubernetes pods if you have access to a cluster and can create pods.
In addition, a container registry is needed that your local image builder can push to and the cluster can pull from.

Configure the [`kubernetes`](#kubernetes-1) field:

```jsonc
// .rumpelpod.json
{
  "kubernetes": {
    "context": "my-cluster",
    "registry": "ghcr.io/me/rumpelpod"
  }
}
```

Images are built locally with Docker or Podman (or on a Docker `buildx` builder if configured) and pushed to the registry.
The local image builder must be authenticated to push to the registry and to read image metadata from it, because rumpelpod queries the registry to decide whether an existing image can be reused or a rebuild is needed.
The cluster must be able to pull from the same registry.

### Container security

Standard Docker containers share the host kernel and are not a strong isolation boundary.
Assume that a process inside the container can escape to the underlying machine.
This is especially relevant when running on localhost, where an escape gives access to your development machine directly.
On a remote machine it still matters if the machine holds sensitive data or runs pods from multiple users.

Rumpelpod supports alternative container runtimes through the devcontainer `runArgs` field:

```jsonc
// .devcontainer/devcontainer.json
{ "runArgs": ["--runtime=runsc"] }
```

[gVisor](https://gvisor.dev/) (`runsc`) intercepts system calls with an application kernel, limiting the kernel's attack surface.
[Kata Containers](https://katacontainers.io/) runs each container inside a lightweight VM.
[Sysbox](https://github.com/nestybox/sysbox) (`sysbox-runc`) is another option, particularly useful when Docker in Docker is needed inside the pod.
On Kubernetes, rumpelpod maps the `--runtime` flag to the pod's `runtimeClassName`.

## Advanced usage

### Git synchronization

Each pod has its own branch, named after the pod and checked out in the container.
`rumpel review` and `rumpel merge` operate on that branch, and every commit inside the pod is automatically pushed to the local repository, where it appears as `rumpelpod/<pod>`.
Secondary branches (any branch whose name differs from the pod name) are available as `rumpelpod/<branch>@<pod>`.

Inside a pod, the local repository's branches are visible as the `host` remote, and other pods' branches are visible as the `rumpelpod` remote under the same naming scheme as on the local machine.
A pod can fetch another pod's work with `git fetch rumpelpod`.

Synchronization goes through the local machine, so remote pods cannot push or see each other's branches while the local machine is offline.
Reconnecting to a pod (via `rumpel enter` or `rumpel claude`) automatically pushes any commits that were made while disconnected.

### Forking pods

`rumpel fork` creates a new pod from the current state of an existing pod:

```sh
rumpel fork prototype auth-chunk
rumpel fork prototype api-chunk
```

The new pods inherit the source pod's working tree and agent state.
They do not inherit gitignored files, or files added outside the checkout after pod creation, such as manually installed system packages.

Forking is useful when one pod reaches a point where the remaining work should split into separate changes.
For example, start with one agent to investigate a feature or rough out a broad change, then fork that pod into two or more pods and ask each agent to own one chunk.
Each fork starts from the same context and continues independently.

### Port forwarding

```jsonc
// .devcontainer/devcontainer.json
{ "forwardPorts": [3000, 8080] }
```

`forwardPorts` in `devcontainer.json` exposes container ports to the local machine.
Prefer it over `appPort` or raw `runArgs: ["-p", ...]` entries, because rumpelpod tracks forwarded ports and remaps them when several pods request the same port.
The remapped ports are shown by `rumpel ports`.

### Copying files

`rumpel cp` transfers files between the local machine and a pod:

```sh
rumpel cp my-pod:/path/in/pod ./local/path
```

Exactly one side of the copy must use `POD:PATH` syntax.
Relative paths on the pod side are resolved relative to the repository root inside the container.

### The DESCRIPTION file

By default, rumpelpod instructs the agent to commit a `DESCRIPTION` file at the repo root that describes the pod branch.
When `rumpel merge` folds the branch back, the contents of that file become the merge commit message and the file is removed from the tree.

The behavior is controlled by the `merge` section of `.rumpelpod.json`.
It can be made strict (fail the merge when the file is missing) or disabled entirely.
The file path is also configurable.

### SSH key forwarding

Rumpelpod runs a per-pod SSH agent on the local machine and relays connections into the container.
Keys are added locally:

```sh
rumpel ssh-add my-pod ~/.ssh/id_ed25519
rumpel ssh-add my-pod -l
```

Arguments after the pod name are forwarded verbatim to `ssh-add`.

Inside the container, `SSH_AUTH_SOCK` points to the relay socket.
The private key material never enters the container.

### Rebuilding the image

Rumpelpod detects some changes that require an image rebuild, such as modified environment variables or files in the build context.
This detection is not perfect.
To force a rebuild, run:

```sh
rumpel image build
```

Add `--pull` to fetch a fresh base image, or `--no-cache` to discard Docker's layer cache entirely.
The latter is useful when [baked build caches](#warm-build-caches) have gone stale.

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/nvidia/rumpelpod/master/install.sh | sh
rumpel system-install
```

The install script fetches the latest release tarball from [GitHub Releases](https://github.com/nvidia/rumpelpod/releases) and extracts it into `~/.local/bin/`, symlinking `rumpel` to the binary for your local machine's platform.
The tarball ships binaries for all supported architectures; they all must live in the same directory, because rumpelpod copies a matching Linux binary into every pod container regardless of your local machine's platform.

`rumpel system-install` installs a user-level background daemon that handles pod lifecycle, git synchronization, and related plumbing.
It runs as a systemd user service on Linux and a launchd user agent on macOS.
The `rumpel` CLI talks to it over a local socket.
Local state is tracked in a SQLite database under `~/.local/state/rumpelpod/` (or `$XDG_STATE_HOME`).

## `.rumpelpod.json` reference

`.rumpelpod.json` lives at the repository root.
It is optional and holds pod-specific settings that have no devcontainer equivalent.
Like `devcontainer.json`, the file is parsed as [JSON5](https://json5.org/), so comments and trailing commas are allowed.

### `host`

```json
{ "host": "ssh://dev-box" }
```

Where to create the container.
Accepts `"localhost"` (the default) or `"ssh://[user@]host"`.
The SSH target is passed to OpenSSH. Configure ports and other SSH-specific settings in `~/.ssh/config`.
Mutually exclusive with `kubernetes`.

### `containerEngine`

```json
{ "containerEngine": "podman" }
```

Selects the local container engine and local image builder.
Allowed values are `"auto"` (default), `"docker"`, and `"podman"`.
`"auto"` tries Docker first for existing behavior, then falls back to Podman when Docker is not installed.

For Kubernetes, this controls the local build-and-push path when `kubernetes.builder` is not set.
If `kubernetes.builder` is set, Docker buildx is required.

### `kubernetes`

```json
{
  "kubernetes": {
    "context": "my-cluster",
    "namespace": "rumpelpods",
    "registry": "ghcr.io/me/rumpelpod",
    "nodeSelector": { "pool": "dev" },
    "tolerations": [
      { "key": "pool", "value": "dev", "effect": "NoSchedule" }
    ],
    "builder": "buildkitd"
  }
}
```

Runs pods as Kubernetes pods instead of Docker containers.
Mutually exclusive with `host`.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `context` | string | required | kubeconfig context name |
| `registry` | string | required | container registry used for pod images |
| `namespace` | string | `"default"` | |
| `nodeSelector` | map<string,string> | none | labels that pods must match to be scheduled |
| `tolerations` | array of toleration | none | see below |
| `builder` | string | none | name of a pre-existing `docker buildx` builder; without it, images build locally with `containerEngine` and are then pushed |

Each toleration is a [Kubernetes toleration][kubernetes-toleration] with the same field names.
`key` and `effect` are required; `value` and `operator` are optional.

[kubernetes-toleration]: https://kubernetes.io/docs/concepts/scheduling-eviction/taint-and-toleration/

### `claude`

```json
{ "claude": { "dangerouslySkipPermissions": false } }
```

The `claude` object configures the `rumpel claude` command.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `dangerouslySkipPermissions` | bool | `true` | passes `--dangerously-skip-permissions` to the claude CLI. The pod already provides an isolated environment, so Claude does not need its own tool-call approval flow on top. |

### `codex`

```json
{ "codex": { "dangerouslyBypassApprovalsAndSandbox": false } }
```

The `codex` object configures the `rumpel codex` command.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `dangerouslyBypassApprovalsAndSandbox` | bool | `true` | passes `--dangerously-bypass-approvals-and-sandbox` to the codex TUI. The pod already provides an isolated environment, so codex does not need its own sandbox on top. |

### `pi`

```json
{ "pi": { "trustWorkspace": false } }
```

The `pi` object configures the `rumpel pi` command.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `trustWorkspace` | bool | `true` | pre-trusts the workspace (`defaultProjectTrust: "always"`) so pi's TUI does not block on the project-trust prompt. The pod already provides an isolated environment. |

### `injectSystemPrompt`

```json
{ "injectSystemPrompt": false }
```

Whether to write a rumpelpod-aware system prompt into each installed agent's prompt location (`/etc/claude-code/CLAUDE.md` for Claude, `~/.codex/AGENTS.md` for codex, `~/.pi/agent/SYSTEM.md` for pi) so the agent knows about the devcontainer layout, git remotes, and the push-on-commit flow.
The description is identical for every agent, so this is a single pod-level switch rather than a per-agent one.
Defaults to `true`.

### `merge`

```json
{
  "merge": {
    "description": "required",
    "descriptionFile": "MERGE_MSG"
  }
}
```

The `merge` object controls rumpelpod's `DESCRIPTION` file convention.
By default, rumpelpod instructs the agent to keep a `DESCRIPTION` file at the repo root that describes the pod branch.
When `rumpel merge` folds the branch back into the working branch on your local machine, the contents of that file are used as the merge commit message and the file itself is removed from the tree.
The convention can be made strict so a missing file fails the merge, or disabled entirely.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `description` | `"auto"` \| `"required"` \| `"off"` | `"auto"` | `"auto"` uses the file when it exists and skips the feature silently when it does not; `"required"` fails the merge when the file is missing; `"off"` disables the feature entirely |
| `descriptionFile` | string | `"DESCRIPTION"` | path of the file within the pod branch, relative to the repo root. Ignored when `description` is `"off"`. |
