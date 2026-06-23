// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Build a "prepared" Docker image that pre-installs the rumpel binary,
//! a repo clone, and (optionally) the Claude CLI on top of the resolved
//! devcontainer image.  This avoids repeating expensive setup steps every
//! time a container is created.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use indoc::{formatdoc, indoc};
use sha2::{Digest, Sha256};

use crate::cli::PrepareImageCommand;
use crate::config::Host;
use crate::git::GitRemote;
use crate::image::{BuildOutputFn, BuildResult, BuildxMode, Image};

/// GCS bucket hosting Claude Code releases (mirrors pod/pty.rs).
const CLAUDE_CODE_DIST_BUCKET: &str =
    "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases";

/// Bump this when the Dockerfile template changes in a way that
/// invalidates previously built prepared images.
const SCHEMA_VERSION: u32 = 8;

/// File baked into the prepared image listing the container env var
/// names the daemon resolved from `containerEnv` and `--env-file` at
/// launch time.  One sorted name per line.  Used by the pod server's
/// `/container-env` endpoint to filter the process environment down
/// to those keys, so `rumpel fork` can snapshot only user-configured
/// env vars without leaking incidental ones (PATH, HOME, ...).
pub const CONTAINER_ENV_KEYS_PATH: &str = "/opt/rumpelpod/container-env-keys";

/// Where the host .git dir is bind-mounted during `docker build`.
/// Must match the `--mount` target in `generate_dockerfile`.
const BUILD_GIT_DIR_PATH: &str = "/tmp/host-git-dir";

const CLI_DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CLI_DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Remotes managed by rumpelpod itself.  These point to local URLs
/// that change across daemon restarts (and test runs), so they must
/// not influence the prepared image tag.
const MANAGED_REMOTES: &[&str] = &["host", "rumpelpod"];

/// Information about the Claude CLI on the local machine, used to pin
/// the exact version inside the prepared image.
struct LocalClaudeInfo {
    version: String,
}

/// Whether the Codex CLI on the local machine works.
///
/// Uses the client-provided path so the daemon does not depend on its
/// own PATH.  None means the client could not find codex and we
/// should not pre-install it into the prepared image.
fn local_has_codex(codex_cli_path: Option<&Path>) -> bool {
    let bin = match codex_cli_path {
        Some(path) => path,
        None => return false,
    };
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether a Codex CLI is already available inside the build container.
fn container_has_codex() -> bool {
    let bin_path = Path::new(crate::daemon::CODEX_CONTAINER_BIN);
    if bin_path.exists() {
        return true;
    }

    Command::new("codex")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Build-time version string that changes whenever the source or git
/// state changes.  Used instead of hashing the (potentially huge)
/// binary at runtime.
const RUMPEL_VERSION_INFO: &str = env!("RUMPELPOD_VERSION_INFO");

/// Try to detect Claude CLI on the local machine and return its version.
///
/// Uses the client-provided path if available, falling back to a
/// PATH search (for backwards compatibility with older clients).
/// `claude --version` outputs e.g. "2.1.79 (Claude Code)"; we
/// extract just the semver portion.
fn detect_local_claude(claude_cli_path: Option<&Path>) -> Option<LocalClaudeInfo> {
    let bin = match claude_cli_path {
        Some(path) => path.to_path_buf(),
        None => PathBuf::from("claude"),
    };
    let output = Command::new(&bin)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // Take only the first whitespace-delimited token (the version number).
    let version = raw.split_whitespace().next()?.to_string();
    if version.is_empty() {
        return None;
    }
    Some(LocalClaudeInfo { version })
}

/// Find all `rumpel-linux-{amd64,arm64}` binaries next to the running
/// executable.  Returns (filename, full_path) pairs.
///
/// Canonicalizes `current_exe` first because on macOS `current_exe`
/// preserves the invocation path (not the underlying inode), so a
/// rumpel installed via symlink would otherwise look in the symlink's
/// directory instead of the directory holding the real binary and
/// its cross-arch siblings.
fn find_rumpel_binaries() -> Result<Vec<(String, PathBuf)>> {
    let invocation = std::env::current_exe().context("resolving own binary path")?;
    let exe = std::fs::canonicalize(&invocation)
        .with_context(|| format!("canonicalizing {}", invocation.display()))?;
    let exe_dir = exe.parent().context("resolving executable directory")?;

    let mut binaries = Vec::new();
    for name in ["rumpel-linux-amd64", "rumpel-linux-arm64"] {
        let path = exe_dir.join(name);
        if path.exists() {
            binaries.push((name.to_string(), path));
        }
    }
    if binaries.is_empty() {
        return Err(anyhow::anyhow!(
            "no rumpel-linux-* binaries found next to {}",
            exe.display()
        ));
    }
    Ok(binaries)
}

/// Compute a deterministic tag for the prepared image.
///
/// Inputs hashed: base image tag, rumpel version, container repo path,
/// user, Claude CLI version (if available), codex install flag,
/// user-configured remotes, schema version, and the resolved
/// `containerEnv` key set (names only, not values, so changing a value
/// in `--env-file` does not force a rebuild).
#[allow(clippy::too_many_arguments)]
fn compute_prepared_tag(
    base_image: &str,
    container_repo_path: &Path,
    container_user: &str,
    claude_info: Option<&LocalClaudeInfo>,
    install_codex: bool,
    host_remotes: &[GitRemote],
    mount_targets: &[String],
    inject_system_prompt: bool,
    description_file: Option<&str>,
    raw_devcontainer_json: &str,
    container_env_keys: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_image.as_bytes());
    hasher.update(RUMPEL_VERSION_INFO.as_bytes());
    hasher.update(container_repo_path.as_os_str().as_encoded_bytes());
    hasher.update(container_user.as_bytes());
    if let Some(info) = claude_info {
        hasher.update(info.version.as_bytes());
    }
    hasher.update([u8::from(install_codex)]);
    for remote in host_remotes {
        if MANAGED_REMOTES.contains(&remote.name.as_str()) {
            continue;
        }
        hasher.update(remote.name.as_bytes());
        hasher.update(b"=");
        hasher.update(remote.url.as_bytes());
        hasher.update(b"\0");
    }
    for target in mount_targets {
        hasher.update(target.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(SCHEMA_VERSION.to_le_bytes());
    hasher.update([u8::from(inject_system_prompt)]);
    if let Some(path) = description_file {
        hasher.update(path.as_bytes());
    }
    // The raw devcontainer.json is baked into the image.  Hash it so
    // the cache invalidates when any config field changes.
    hasher.update(raw_devcontainer_json.as_bytes());
    // Resolved containerEnv key names (after env-file resolution).
    // Hashing names only -- not values -- means rotating a secret in
    // an `--env-file` does not invalidate the image.  Adding or
    // removing a key does.
    for key in container_env_keys {
        hasher.update(key.as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(&hasher.finalize()[..8])
}

/// Generate the Dockerfile content for the prepared image.
///
/// The host `.git` dir is passed as a named build context (`gateway`)
/// and bind-mounted at build time so its contents never end up in an
/// image layer.
///
/// After the rumpel binary is installed, remaining setup (repo clone,
/// Claude CLI) is delegated to `rumpel prepare-image` so the logic
/// lives in Rust instead of shell.
#[allow(clippy::too_many_arguments)]
fn generate_dockerfile(
    base_image: &str,
    container_repo_path: &Path,
    container_user: &str,
    claude_info: Option<&LocalClaudeInfo>,
    install_codex: bool,
    host_remotes: &[GitRemote],
    mount_targets: &[String],
    inject_system_prompt: bool,
    description_file: Option<&str>,
) -> String {
    let repo_path = container_repo_path.display();
    let rumpel = crate::daemon::RUMPEL_CONTAINER_BIN;

    let claude_flag = match claude_info {
        Some(info) => format!(" \\\n      --claude-version '{}'", info.version),
        None => String::new(),
    };

    let codex_flag = if install_codex {
        " \\\n      --install-codex"
    } else {
        ""
    };

    let remote_flags: String = host_remotes
        .iter()
        .filter(|r| !MANAGED_REMOTES.contains(&r.name.as_str()))
        .map(|r| format!(" \\\n      --remote '{}={}'", r.name, r.url))
        .collect();

    let mount_target_flags: String = mount_targets
        .iter()
        .map(|t| format!(" \\\n      --mount-target '{t}'"))
        .collect();

    let system_prompt_flag = if inject_system_prompt {
        " \\\n      --inject-system-prompt"
    } else {
        ""
    };

    let description_flag = match description_file {
        Some(path) => format!(" \\\n      --description-file '{path}'"),
        None => String::new(),
    };

    // The image must end with the base image's original USER so that
    // the ENTRYPOINT/CMD runs as the user the base image intended.
    // This is distinct from the container user (remoteUser /
    // containerUser in devcontainer.json) which only affects exec
    // sessions -- switch_user() handles dropping to the container
    // user in-pod.
    formatdoc! {r#"
        FROM {base_image}

        ARG TARGETARCH
        ARG BASE_USER=root
        USER root

        COPY --chmod=755 rumpel-linux-${{TARGETARCH}} {rumpel}
        COPY devcontainer.json /opt/rumpelpod/devcontainer.json
        COPY container-env-keys {CONTAINER_ENV_KEYS_PATH}

        RUN --mount=type=bind,from=gateway,target={BUILD_GIT_DIR_PATH} \
            {rumpel} prepare-image \
              --repo-path '{repo_path}' \
              --user '{container_user}'{claude_flag}{codex_flag}{remote_flags}{mount_target_flags}{system_prompt_flag}{description_flag}

        USER ${{BASE_USER}}
    "#}
}

/// Assemble the build context directory with the Dockerfile and binaries.
///
/// The host `.git` dir is NOT included here -- it is passed separately
/// via `--build-context` so buildx can transfer it efficiently and the
/// Dockerfile bind-mounts it without baking it into a layer.
#[allow(clippy::too_many_arguments)]
fn assemble_build_context(
    base_image: &str,
    container_repo_path: &Path,
    container_user: &str,
    claude_info: Option<&LocalClaudeInfo>,
    install_codex: bool,
    host_remotes: &[GitRemote],
    mount_targets: &[String],
    inject_system_prompt: bool,
    description_file: Option<&str>,
    raw_devcontainer_json: &str,
    container_env_keys: &[String],
) -> Result<tempfile::TempDir> {
    let dockerfile = generate_dockerfile(
        base_image,
        container_repo_path,
        container_user,
        claude_info,
        install_codex,
        host_remotes,
        mount_targets,
        inject_system_prompt,
        description_file,
    );
    let binaries = find_rumpel_binaries()?;

    let tmp = tempfile::tempdir().context("creating build context temp dir")?;
    fs::write(tmp.path().join("Dockerfile"), dockerfile)
        .context("writing Dockerfile to build context")?;
    for (name, path) in &binaries {
        fs::copy(path, tmp.path().join(name)).with_context(|| {
            let path = path.display();
            format!("copying {path} to build context")
        })?;
    }

    fs::write(tmp.path().join("devcontainer.json"), raw_devcontainer_json)
        .context("writing devcontainer.json to build context")?;

    // One key per line, sorted by the caller.  Empty file is fine --
    // the COPY in the Dockerfile still creates the file inside the
    // image so the pod server can unconditionally read it.
    let mut keys_file = String::new();
    for key in container_env_keys {
        keys_file.push_str(key);
        keys_file.push('\n');
    }
    fs::write(tmp.path().join("container-env-keys"), keys_file)
        .context("writing container-env-keys to build context")?;

    Ok(tmp)
}

/// Create a symlink to the `.git` dir without the `.git` suffix.
///
/// Buildx misidentifies directories containing a HEAD file as remote
/// git URLs.  A symlink without `.git` in the name forces
/// local-directory treatment.
fn create_gateway_link(git_dir: &Path, build_context: &Path) -> Result<PathBuf> {
    let resolved = fs::canonicalize(git_dir).with_context(|| {
        let dir = git_dir.display();
        format!("resolving git dir {dir}")
    })?;
    let gateway_link = build_context.join("gateway-link");
    std::os::unix::fs::symlink(&resolved, &gateway_link).with_context(|| {
        let target = resolved.display();
        format!("symlinking gateway-link -> {target}")
    })?;
    Ok(gateway_link)
}

/// Tag an image by its sha256 digest so `docker build FROM` can reference it.
///
/// `docker build` cannot resolve bare `sha256:...` image IDs in FROM
/// lines.  Tagging it with a friendly name works around this.
fn ensure_buildable_tag(
    image: &str,
    docker_host: &Host,
    docker_socket: Option<&Path>,
) -> Result<String> {
    if !image.starts_with("sha256:") {
        return Ok(image.to_string());
    }
    let friendly = format!("rumpelpod-base-{}", &image[7..23]);
    let mut cmd = Command::new("docker");
    crate::image::apply_docker_host(&mut cmd, docker_host, docker_socket);
    cmd.args(["tag", image, &friendly]);
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("tagging base image for prepared build")?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "failed to tag image '{image}' as '{friendly}'"
        ));
    }
    Ok(friendly)
}

/// Build (or reuse a cached) prepared image on top of `base_image`.
///
/// The prepared image includes the rumpel binary, a git clone of the
/// repo from the host `.git` dir, and optionally the Claude CLI.
///
/// Returns a `BuildResult` indicating the final image tag and whether
/// a build actually ran.
#[allow(clippy::too_many_arguments)]
/// Build a prepared image on top of `base_image`.
///
/// Two user concepts are in play:
///
/// - **image user** (`image_user`): the USER directive from the base
///   image's Dockerfile.  Read from the image config.
///
/// - **container user** (`container_user`): the user who owns the
///   workspace and runs commands at runtime.  Comes from
///   `remoteUser`/`containerUser` in devcontainer.json, falling back
///   to `image_user` when neither is set.  Baked into the image at
///   `/opt/rumpelpod/user` so `container-exec` and `container-serve`
///   can switch to it at runtime.
pub fn build_prepared_image(
    base_image: &Image,
    docker_host: &Host,
    git_dir: &Path,
    container_repo_path: &Path,
    container_user: Option<&str>,
    host_remotes: &[GitRemote],
    mount_targets: &[String],
    claude_cli_path: Option<&Path>,
    codex_cli_path: Option<&Path>,
    docker_socket: Option<&Path>,
    inject_system_prompt: bool,
    description_file: Option<&str>,
    raw_devcontainer_json: &str,
    container_env_keys: &[String],
    build_options: &[String],
    ssh_auth_sock: Option<&Path>,
    on_output: Option<BuildOutputFn>,
) -> Result<BuildResult> {
    let claude_info = detect_local_claude(claude_cli_path);
    let install_codex = local_has_codex(codex_cli_path);

    let mode = BuildxMode::from_host(docker_host, docker_socket);

    let image_user = crate::image::inspect_image_user(&base_image.0, docker_host, docker_socket)?;

    let buildable_base = match &mode {
        BuildxMode::Load {
            docker_host,
            docker_socket,
        } => ensure_buildable_tag(&base_image.0, docker_host, *docker_socket)?,
        BuildxMode::Push { .. } => base_image.0.clone(),
    };

    // Resolve the container user: explicit from devcontainer.json,
    // or fall back to the image's own USER.
    let container_user = container_user.unwrap_or(&image_user);

    // On k8s, kubectl exec has no --user flag, so we enter the
    // container as the image's USER.  switch_user() can only drop
    // privileges (root -> non-root) or no-op (already the right
    // user).  Any other combination is unsupported.
    if matches!(docker_host, Host::Kubernetes { .. })
        && image_user != "root"
        && container_user != image_user
    {
        return Err(anyhow::anyhow!(
            "Kubernetes does not support containerUser/remoteUser '{container_user}' \
             when the image USER is '{image_user}' (not root).\n\
             Either set the image USER to root or remove the \
             containerUser/remoteUser override from devcontainer.json."
        ));
    }

    let tag = compute_prepared_tag(
        &base_image.0,
        container_repo_path,
        container_user,
        claude_info.as_ref(),
        install_codex,
        host_remotes,
        mount_targets,
        inject_system_prompt,
        description_file,
        raw_devcontainer_json,
        container_env_keys,
    );

    // Check cache: for Push mode, check the registry first, then
    // fall back to a local copy that just needs pushing.  For Load
    // mode, check the target docker host.
    match &mode {
        BuildxMode::Push { registry, .. } => {
            let push_tag = format!("{registry}:{tag}");
            if crate::image::registry_image_exists(&push_tag)? {
                return Ok(BuildResult {
                    image: Image(mode.output_tag(&tag)),
                    built: false,
                });
            }
        }
        BuildxMode::Load {
            docker_host,
            docker_socket,
        } => {
            let local_tag = mode.output_tag(&tag);
            if crate::image::image_exists(&local_tag, docker_host, *docker_socket) {
                return Ok(BuildResult {
                    image: Image(local_tag),
                    built: false,
                });
            }
        }
    }

    let build_ctx = assemble_build_context(
        &buildable_base,
        container_repo_path,
        container_user,
        claude_info.as_ref(),
        install_codex,
        host_remotes,
        mount_targets,
        inject_system_prompt,
        description_file,
        raw_devcontainer_json,
        container_env_keys,
    )?;

    let gateway_link = create_gateway_link(git_dir, build_ctx.path())?;
    let gateway_link_display = gateway_link.display().to_string();

    let mut extra_args = vec![
        format!("--build-context=gateway={gateway_link_display}"),
        format!("--build-arg=BASE_USER={image_user}"),
    ];
    extra_args.extend(build_options.iter().cloned());

    let dockerfile = build_ctx.path().join("Dockerfile");

    crate::image::run_buildx_build(
        &tag,
        &dockerfile,
        build_ctx.path(),
        &mode,
        &extra_args,
        on_output,
        ssh_auth_sock,
    )?;

    Ok(BuildResult {
        image: Image(mode.output_tag(&tag)),
        built: true,
    })
}

// ---------------------------------------------------------------------------
// `rumpel prepare-image` subcommand -- runs inside the container at build time
// ---------------------------------------------------------------------------

/// Clone the repo and optionally install the Claude CLI.
///
/// Invoked as a Dockerfile RUN step after the rumpel binary itself has
/// been copied in.  Replaces what used to be shell scripting.
///
/// `cmd.user` is the **container user** -- the user who will own the
/// workspace and run commands at runtime (from remoteUser/containerUser,
/// or the image's USER if neither is set).  This is distinct from the
/// **image user** (the USER directive in the Dockerfile), which controls
/// who runs the entrypoint.  The two coincide when devcontainer.json
/// does not set remoteUser/containerUser.
pub fn run_prepare_image(cmd: &PrepareImageCommand) -> Result<()> {
    if crate::which("git").is_none() {
        return Err(anyhow::anyhow!(
            "the devcontainer image does not have git installed. \
             Please add it to your Dockerfile (e.g. `apt-get install -y git`)"
        ));
    }

    if !cmd.repo_path.join(".git").exists() {
        let status = Command::new("git")
            .args(["clone", &format!("file://{BUILD_GIT_DIR_PATH}")])
            .arg(&cmd.repo_path)
            .status()
            .context("cloning repository from build-time git dir")?;
        if !status.success() {
            return Err(anyhow::anyhow!("git clone failed"));
        }
    }

    // If the base image already had a .git (e.g. from COPY), it may
    // contain host-side hooks that reference binaries outside the
    // container.  Remove them so the pod server can install its own.
    remove_host_hooks(&cmd.repo_path)?;

    // Ensure the repo is owned by the container user.  The base image
    // may have created it under a different UID (e.g. COPY --chown).
    // Trailing colon makes chown use the user's primary group.
    let user_colon = format!("{}:", cmd.user);
    let status = Command::new("chown")
        .args(["-R", &user_colon])
        .arg(&cmd.repo_path)
        .status()
        .context("setting repository ownership")?;
    if !status.success() {
        return Err(anyhow::anyhow!("chown failed"));
    }

    // TODO: this is a workaround -- the chown above should make
    // ownership correct, but configure_remotes runs later as root
    // and may create root-owned files inside .git.  Figure out why
    // the ownership ends up wrong and remove this.
    //
    // Mark the repo as safe for all users so git does not reject
    // ownership mismatches.  prepare-image runs as root but the
    // container user is different; system-level config applies to both.
    let repo_path_str = cmd.repo_path.display().to_string();
    let status = Command::new("git")
        .args([
            "config",
            "--system",
            "--add",
            "safe.directory",
            &repo_path_str,
        ])
        .status()
        .context("setting safe.directory")?;
    if !status.success() {
        return Err(anyhow::anyhow!("git config safe.directory failed"));
    }

    // Configure host remotes in the cloned repo so they match the
    // host's configuration from the start.
    configure_remotes(&cmd.repo_path, &cmd.remotes)?;

    if let Some(ref version) = cmd.claude_version {
        install_claude_cli(version)?;
    }

    if cmd.install_codex {
        install_codex_cli()?;
    }

    if cmd.inject_system_prompt {
        write_system_prompt(cmd.description_file.as_deref())?;
        if container_has_codex() {
            write_codex_system_prompt(&cmd.user, cmd.description_file.as_deref())?;
        }
    }

    if let Some(ref description_file) = cmd.description_file {
        install_pre_commit_hook(&cmd.repo_path, description_file)?;
    }

    // Record the resolved container user so container-exec and
    // container-serve can switch to it at runtime.
    fs::write(crate::switch_user::USER_FILE, &cmd.user)
        .context("writing container user to /opt/rumpelpod/user")?;

    // Let the container user write to /opt/rumpelpod (e.g. the server token
    // file).  The binary keeps its 755 permissions regardless of owner.
    let user_and_group = format!("{user}:", user = cmd.user);
    let status = Command::new("chown")
        .args(["-R", &user_and_group, "/opt/rumpelpod"])
        .status()
        .context("setting /opt/rumpelpod ownership")?;
    if !status.success() {
        return Err(anyhow::anyhow!("chown /opt/rumpelpod failed"));
    }

    create_mount_targets(&cmd.mount_targets, &cmd.user)?;

    Ok(())
}

/// Pre-create each mount target directory owned by the container user.
///
/// Docker synthesizes a root-owned directory at the mount target when
/// one does not already exist in the image, so a fresh volume or tmpfs
/// ends up root-owned and unwritable by a non-root image USER.  For
/// volumes, the initial permissions are also copied from the target
/// directory, so pre-creating it with the right owner fixes both the
/// mount point itself and the volume contents on first use.
fn create_mount_targets(targets: &[String], user: &str) -> Result<()> {
    if targets.is_empty() {
        return Ok(());
    }

    let pw = nix::unistd::User::from_name(user)
        .with_context(|| format!("looking up user '{user}'"))?
        .with_context(|| format!("user '{user}' not found in /etc/passwd"))?;

    for target in targets {
        fs::create_dir_all(target)
            .with_context(|| format!("creating mount target dir '{target}'"))?;
        std::os::unix::fs::chown(target, Some(pw.uid.as_raw()), Some(pw.gid.as_raw()))
            .with_context(|| format!("chowning mount target '{target}' to '{user}'"))?;
    }
    Ok(())
}

/// Generate the system prompt describing the rumpelpod environment.
///
/// When `description_file` is Some, includes instructions telling the
/// agent to write the merge commit message into that file.
pub fn system_prompt(description_file: Option<&str>) -> String {
    let mut prompt = indoc! {"
        You are running inside a rumpelpod, an isolated devcontainer.

        Git remotes:
        `host/` has branches from the host repo.
        `rumpelpod/` has branches from other pods on the same repo.

        Always commit your changes. The user interacts with your work via git, not by looking at the working tree.
        Committing automatically pushes to the host repo.
        Fetching from these remotes is not automatic; run `git fetch` explicitly when you need updates.
    "}
    .to_string();

    if let Some(path) = description_file {
        prompt.push_str(&formatdoc! {"

            Commit a `{path}` file at the repo root, formatted like a git commit message, that describes your branch for the merge commit message.
            Create it with your first commit and update it as your work evolves.
            The host will use its contents as the merge commit message and remove the file after merging.
        "});
    }

    prompt
}

/// Write /etc/claude-code/CLAUDE.md so Claude understands the container
/// layout and git remote conventions.
fn write_system_prompt(description_file: Option<&str>) -> Result<()> {
    let dir = Path::new("/etc/claude-code");
    fs::create_dir_all(dir).context("creating /etc/claude-code")?;
    fs::write(dir.join("CLAUDE.md"), system_prompt(description_file))
        .context("writing /etc/claude-code/CLAUDE.md")
}

/// Append the rumpelpod system prompt to the container user's
/// `~/.codex/AGENTS.md`.  Codex loads this global instructions file for
/// every session regardless of cwd; a repo-tree AGENTS.md would only be
/// read inside the project root, and a filesystem-root `/AGENTS.md` is
/// above the project root and never read.  Appends rather than
/// overwrites so a base image's existing file is preserved.
fn write_codex_system_prompt(user: &str, description_file: Option<&str>) -> Result<()> {
    let pw = nix::unistd::User::from_name(user)
        .with_context(|| format!("looking up user '{user}'"))?
        .with_context(|| format!("user '{user}' not found in /etc/passwd"))?;
    let codex_dir = pw.dir.join(".codex");
    fs::create_dir_all(&codex_dir).with_context(|| {
        let d = codex_dir.display();
        format!("creating {d}")
    })?;
    let path = codex_dir.join("AGENTS.md");

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| {
            let p = path.display();
            format!("opening {p} for append")
        })?;
    let prompt = system_prompt(description_file);
    // Separate from any existing content with a blank line.
    if path.metadata().is_ok_and(|m| m.len() > 0) {
        file.write_all(b"\n")
            .context("writing AGENTS.md separator")?;
    }
    file.write_all(prompt.as_bytes())
        .context("writing rumpelpod prompt to AGENTS.md")?;
    drop(file);

    std::os::unix::fs::chown(&codex_dir, Some(pw.uid.as_raw()), Some(pw.gid.as_raw()))
        .with_context(|| {
            let d = codex_dir.display();
            format!("chowning {d}")
        })?;
    std::os::unix::fs::chown(&path, Some(pw.uid.as_raw()), Some(pw.gid.as_raw())).with_context(
        || {
            let p = path.display();
            format!("chowning {p}")
        },
    )
}

/// Write `.git/hooks/pre-commit` in the cloned repo.  The hook fails
/// the commit when the DESCRIPTION file is missing or not formatted
/// like a git commit message.  Signed with a distinct comment so the
/// host-hook stripper and the pod-side reference-transaction installer
/// leave it alone.
fn install_pre_commit_hook(repo_path: &Path, description_file: &str) -> Result<()> {
    let hooks_dir = repo_path.join(".git/hooks");
    fs::create_dir_all(&hooks_dir).with_context(|| {
        let p = hooks_dir.display();
        format!("creating hooks dir {p}")
    })?;
    let hook_path = hooks_dir.join("pre-commit");

    // Single-quote the path for the shell and escape any embedded
    // single quotes so a config-supplied path cannot inject commands.
    let escaped = description_file.replace('\'', "'\\''");
    let content = format!(
        "#!/bin/sh\n\
         # Installed by rumpelpod (pod pre-commit)\n\
         exec /opt/rumpelpod/bin/rumpel git-hook pre-commit-description --file '{escaped}'\n"
    );

    fs::write(&hook_path, content).with_context(|| {
        let p = hook_path.display();
        format!("writing pre-commit hook {p}")
    })?;
    let mut perms = fs::metadata(&hook_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook_path, perms).with_context(|| {
        let p = hook_path.display();
        format!("setting mode on {p}")
    })?;
    Ok(())
}

/// Parse "NAME=URL" remote specs and add/update them in the repo.
///
/// Also removes any pre-existing remotes (from the base image) that
/// are not in the provided list and not rumpelpod-managed.
fn configure_remotes(repo_path: &Path, remote_specs: &[String]) -> Result<()> {
    if remote_specs.is_empty() {
        return Ok(());
    }

    let parsed: Vec<(&str, &str)> = remote_specs
        .iter()
        .map(|s| {
            s.split_once('=')
                .with_context(|| format!("invalid --remote spec '{s}', expected NAME=URL"))
        })
        .collect::<Result<_>>()?;

    // List existing remotes in the repo.
    let existing_output = Command::new("git")
        .args(["remote"])
        .current_dir(repo_path)
        .output()
        .context("listing existing remotes")?;
    let existing: Vec<&str> = std::str::from_utf8(&existing_output.stdout)
        .context("non-UTF-8 remote names")?
        .lines()
        .collect();

    let managed = MANAGED_REMOTES;

    // Remove stale remotes that are not in the host list and not managed.
    for name in &existing {
        if managed.contains(name) {
            continue;
        }
        if !parsed.iter().any(|(n, _)| n == name) {
            Command::new("git")
                .args(["remote", "remove", name])
                .current_dir(repo_path)
                .status()
                .with_context(|| format!("removing remote '{name}'"))?;
        }
    }

    // Add or update remotes from the host.
    for (name, url) in &parsed {
        if managed.contains(name) {
            continue;
        }
        if existing.contains(name) {
            Command::new("git")
                .args(["remote", "set-url", name, url])
                .current_dir(repo_path)
                .status()
                .with_context(|| format!("setting URL for remote '{name}'"))?;
        } else {
            Command::new("git")
                .args(["remote", "add", name, url])
                .current_dir(repo_path)
                .status()
                .with_context(|| format!("adding remote '{name}'"))?;
        }
    }

    Ok(())
}

fn download_cli_asset(url: &str, label: &str) -> Result<Vec<u8>> {
    download_cli_asset_with_timeouts(
        url,
        label,
        CLI_DOWNLOAD_CONNECT_TIMEOUT,
        CLI_DOWNLOAD_READ_TIMEOUT,
    )
}

fn cli_download_client_with_timeouts(
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<reqwest::Client> {
    // Release assets can be large enough for active slow transfers to exceed
    // a total request deadline; read_timeout still fails stalled transfers.
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .build()
        .context("building CLI download HTTP client")
}

fn download_cli_asset_with_timeouts(
    url: &str,
    label: &str,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<Vec<u8>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building CLI download runtime")?;

    runtime.block_on(async {
        let client = cli_download_client_with_timeouts(connect_timeout, read_timeout)?;
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("downloading {label} from {url}"))?;
        let response = response
            .error_for_status()
            .with_context(|| format!("downloading {label} from {url}"))?;
        let data = response
            .bytes()
            .await
            .with_context(|| format!("reading {label} from {url}"))?;

        Ok(data.to_vec())
    })
}

/// Download and install the Claude CLI at a specific version.
///
/// Skips if a `claude` binary is already present.
fn install_claude_cli(version: &str) -> Result<()> {
    let bin_path = Path::new(crate::daemon::CLAUDE_CONTAINER_BIN);
    if bin_path.exists() {
        return Ok(());
    }
    if Command::new("claude")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Ok(());
    }

    let platform = match std::env::consts::ARCH {
        "x86_64" => "linux-x64",
        "aarch64" => "linux-arm64",
        other => return Err(anyhow::anyhow!("unsupported architecture '{other}'")),
    };

    let url = format!("{CLAUDE_CODE_DIST_BUCKET}/{version}/{platform}/claude");
    let data = download_cli_asset(&url, "Claude CLI binary")?;

    if let Some(parent) = bin_path.parent() {
        fs::create_dir_all(parent).context("creating /opt/rumpelpod/bin")?;
    }
    fs::write(bin_path, &data).context("writing Claude CLI binary")?;
    fs::set_permissions(bin_path, fs::Permissions::from_mode(0o755))
        .context("making Claude CLI binary executable")?;

    Ok(())
}

/// Download and install the Codex CLI from the GitHub release matching
/// the host architecture.
///
/// Skips if a `codex` binary is already present.
fn install_codex_cli() -> Result<()> {
    if container_has_codex() {
        return Ok(());
    }
    let bin_path = Path::new(crate::daemon::CODEX_CONTAINER_BIN);

    // musl build is static, so it runs on any base image.
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => return Err(anyhow::anyhow!("unsupported architecture '{other}'")),
    };

    let url =
        format!("https://github.com/openai/codex/releases/latest/download/codex-{arch}.tar.gz");
    let data = download_cli_asset(&url, "Codex CLI tarball")?;

    // The tarball contains a single file named codex-<arch>.
    let decoder = flate2::read::GzDecoder::new(&data[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut entry = archive
        .entries()
        .context("reading tar entries")?
        .next()
        .context("empty tarball")?
        .context("reading tar entry")?;

    if let Some(parent) = bin_path.parent() {
        fs::create_dir_all(parent).context("creating /opt/rumpelpod/bin")?;
    }
    std::io::copy(
        &mut entry,
        &mut fs::File::create(bin_path).context("creating codex binary")?,
    )
    .context("extracting codex binary")?;
    fs::set_permissions(bin_path, fs::Permissions::from_mode(0o755))
        .context("making codex binary executable")?;

    Ok(())
}

/// Strip host-side hook lines that were baked into the image (e.g. via
/// `COPY` in the Dockerfile).  Host hooks reference binary paths that
/// do not exist inside the container.  Only the exact lines produced
/// by the host hook shim are removed; any user hook code is preserved.
fn remove_host_hooks(repo_path: &Path) -> Result<()> {
    let hooks_dir = repo_path.join(".git/hooks");
    if !hooks_dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(&hooks_dir).context("reading hooks directory")? {
        let entry = entry.context("reading hooks entry")?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let cleaned = crate::gateway::strip_host_hooks(&content);
        if cleaned == content {
            continue;
        }
        // If the file is now just a shebang (no real code), remove it
        // entirely so the pod server knows this is a first entry and
        // can sanitize malformed checkouts.
        let has_code = cleaned
            .lines()
            .any(|l| !l.is_empty() && !l.starts_with("#!"));
        if has_code {
            fs::write(&path, cleaned).with_context(|| {
                let path = path.display();
                format!("rewriting hook {path}")
            })?;
        } else {
            fs::remove_file(&path).with_context(|| {
                let path = path.display();
                format!("removing empty hook {path}")
            })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    fn serve_slow_active_body(mut stream: TcpStream) -> std::io::Result<()> {
        let mut request = [0_u8; 1024];
        let _bytes_read = stream.read(&mut request)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")?;
        stream.flush()?;

        for byte in b"hello" {
            thread::sleep(Duration::from_millis(50));
            stream.write_all(&[*byte])?;
            stream.flush()?;
        }

        Ok(())
    }

    #[test]
    fn cli_download_client_allows_slow_active_body() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test listener");
        let addr = listener.local_addr().expect("read test listener addr");
        let server = thread::spawn(move || -> std::io::Result<()> {
            let (stream, _) = listener.accept()?;
            serve_slow_active_body(stream)
        });

        let start = Instant::now();
        let url = format!("http://{addr}/cli");
        let data = download_cli_asset_with_timeouts(
            &url,
            "test CLI body",
            Duration::from_secs(1),
            Duration::from_millis(120),
        )
        .expect("download test response body");

        assert_eq!(&data[..], b"hello");
        assert!(
            start.elapsed() >= Duration::from_millis(200),
            "response completed before the test exceeded the read timeout"
        );
        server
            .join()
            .expect("test server thread panicked")
            .expect("test server failed");
    }
}
