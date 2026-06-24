// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Docker image resolution and building.
//!
//! All image builds go through `docker buildx build`.  The only
//! variation between local Docker and Kubernetes is `--load` (keeps
//! the image in the local daemon) vs `--push` (pushes to a registry).

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use walkdir::WalkDir;

use crate::config::Host;
use crate::devcontainer::{BuildOptions, DevContainer};

/// Set the `-H` (docker host) flag on a docker command.
///
/// Local Docker may pass an explicit socket so the command uses the
/// same daemon endpoint rumpelpod resolved earlier.  SSH Docker hosts
/// use Docker's native `ssh://` transport.
pub fn apply_docker_host(cmd: &mut Command, docker_host: &Host, docker_socket: Option<&Path>) {
    if let Some(socket) = docker_socket {
        let socket = socket.display();
        cmd.args(["-H", &format!("unix://{socket}")]);
    } else if let Some(uri) = docker_host.docker_host_uri() {
        cmd.args(["-H", &uri]);
    }
}

/// A line of Docker build output, tagged with the stream it came from.
pub enum OutputLine {
    Stdout(String),
    Stderr(String),
}

/// Callback invoked with each line of Docker build output.
pub type BuildOutputFn = Box<dyn FnMut(OutputLine) + Send>;

// Re-export so callers can use image::Image
pub use crate::daemon::protocol::Image;

/// Result of a build attempt, indicating whether a build actually ran.
pub struct BuildResult {
    pub image: Image,
    pub built: bool,
}

/// Optional flags that control Docker build behavior.
#[derive(Default)]
pub struct BuildFlags {
    /// Pass `--no-cache` to disable Docker layer caching.
    pub no_cache: bool,
    /// Pass `--pull` to pull the base image before building.
    pub pull: bool,
    /// Skip the cache check and always build.
    pub force: bool,
}

/// Whether the build context's filesystem path participates in the
/// image tag.
pub enum ContextPathTagging {
    /// Fold the context path into the tag.  Two byte-identical contexts
    /// at different paths then get distinct tags and distinct images
    /// instead of colliding on one cached image.  This is the normal
    /// case: real devcontainer contexts live at the stable repo root.
    Include,
    /// Leave the context path out of the tag.  The built-in default
    /// image is staged into a throwaway tempdir whose path changes on
    /// every invocation, so including it would never let the cache hit.
    Ignore,
}

/// Whether the build targets a local Docker daemon or a remote registry.
pub(crate) enum BuildxMode<'a> {
    /// `--load` into a local (or SSH-forwarded) Docker daemon.
    Load {
        docker_host: &'a Host,
        docker_socket: Option<&'a Path>,
    },
    /// `--push` to a registry, optionally via a named builder.
    Push {
        registry: &'a str,
        builder: Option<&'a str>,
    },
}

/// Run a `docker buildx build` against an already-staged context
/// and return the registry- or local-qualified tag for the image.
/// `tag` is the content-addressed component; `mode` selects push
/// vs load.  Bypasses `resolve_image`'s devcontainer.json plumbing
/// so callers like `src/hub/image.rs` can drive buildx directly.
pub(crate) fn build_image_direct(
    tag: &str,
    dockerfile: &Path,
    context: &Path,
    mode: &BuildxMode,
) -> Result<String> {
    run_buildx_build(tag, dockerfile, context, mode, &[], None, None)?;
    Ok(mode.output_tag(tag))
}

impl<'a> BuildxMode<'a> {
    pub(crate) fn from_host(host: &'a Host, docker_socket: Option<&'a Path>) -> Self {
        match host {
            Host::Kubernetes {
                registry, builder, ..
            } => BuildxMode::Push {
                registry,
                builder: builder.as_deref(),
            },
            Host::Localhost | Host::Ssh { .. } => BuildxMode::Load {
                docker_host: host,
                docker_socket,
            },
        }
    }

    /// The tag that callers use to reference the built image.
    pub(crate) fn output_tag(&self, local_tag: &str) -> String {
        match self {
            BuildxMode::Load { .. } => format!("rumpelpod-{local_tag}"),
            BuildxMode::Push { registry, .. } => format!("{registry}:{local_tag}"),
        }
    }
}

/// Resolve a Docker image, building from devcontainer.json 'build' if necessary.
///
/// The DevContainer's build paths must already be resolved to repo-root-relative
/// paths (via `resolve_build_paths`) before calling this.
#[allow(clippy::too_many_arguments)]
pub fn resolve_image(
    devcontainer: &DevContainer,
    docker_host: &Host,
    repo_root: &Path,
    flags: &BuildFlags,
    path_tagging: ContextPathTagging,
    on_output: Option<BuildOutputFn>,
    docker_socket: Option<&Path>,
    ssh_auth_sock: Option<&Path>,
) -> Result<BuildResult> {
    let build = match &devcontainer.build {
        Some(build) => build,
        None => {
            return Ok(BuildResult {
                image: Image(
                    devcontainer
                        .image
                        .clone()
                        .expect("either image or build must be set"),
                ),
                built: false,
            });
        }
    };

    let dockerfile = build
        .dockerfile
        .as_deref()
        .expect("resolved build must have dockerfile");
    let dockerfile_path = repo_root.join(dockerfile);

    if !dockerfile_path.exists() {
        let path = dockerfile_path.display();
        return Err(anyhow::anyhow!(
            "devcontainer Dockerfile '{path}' not found"
        ));
    }

    let context = build
        .context
        .as_deref()
        .expect("resolved build must have context");
    let context_path = repo_root.join(context);

    let image_name = compute_image_tag(build, &dockerfile_path, &context_path, path_tagging)?;
    let mode = BuildxMode::from_host(docker_host, docker_socket);

    if !flags.force {
        match &mode {
            BuildxMode::Push { registry, .. } => {
                let push_tag = format!("{registry}:{image_name}");
                if registry_image_exists(&push_tag)? {
                    return Ok(BuildResult {
                        image: Image(mode.output_tag(&image_name)),
                        built: false,
                    });
                }
            }
            BuildxMode::Load {
                docker_host,
                docker_socket,
            } => {
                let local_name = mode.output_tag(&image_name);
                if image_exists(&local_name, docker_host, *docker_socket) {
                    return Ok(BuildResult {
                        image: Image(local_name),
                        built: false,
                    });
                }
            }
        }
    }

    let mut extra_args = collect_build_args(build);
    if flags.no_cache {
        extra_args.push("--no-cache".into());
    }
    if flags.pull {
        extra_args.push("--pull".into());
    }

    run_buildx_build(
        &image_name,
        &dockerfile_path,
        &context_path,
        &mode,
        &extra_args,
        on_output,
        ssh_auth_sock,
    )?;

    Ok(BuildResult {
        image: Image(mode.output_tag(&image_name)),
        built: true,
    })
}

/// Collect `--build-arg`, `--target`, `--cache-from`, and raw options
/// from a devcontainer build spec into a flat list of CLI arguments.
pub(crate) fn collect_build_args(build: &BuildOptions) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(build_args) = &build.args {
        for (k, v) in build_args {
            args.push("--build-arg".into());
            args.push(format!("{k}={v}"));
        }
    }

    if let Some(target) = &build.target {
        args.push("--target".into());
        args.push(target.clone());
    }

    if let Some(cache_from) = &build.cache_from {
        match cache_from {
            crate::devcontainer::StringOrArray::String(s) => {
                args.push("--cache-from".into());
                args.push(s.clone());
            }
            crate::devcontainer::StringOrArray::Array(arr) => {
                for s in arr {
                    args.push("--cache-from".into());
                    args.push(s.clone());
                }
            }
        }
    }

    if let Some(options) = &build.options {
        args.extend(options.iter().cloned());
    }

    args
}

/// Run `docker buildx build` with the given mode and arguments.
///
/// When `on_output` is provided, build output is streamed line-by-line
/// through the callback (used for base image builds where the daemon
/// forwards progress to the client).  Otherwise output is suppressed
/// on success and included in the error on failure.
///
/// `ssh_auth_sock`, when `Some`, is set verbatim as `SSH_AUTH_SOCK`
/// in the child environment; buildx itself decides whether anything
/// in the build (e.g. `--ssh=default`) consumes it.  When `None`,
/// whatever `SSH_AUTH_SOCK` the current process has is inherited.
pub(crate) fn run_buildx_build(
    tag: &str,
    dockerfile: &Path,
    context: &Path,
    mode: &BuildxMode,
    extra_args: &[String],
    on_output: Option<BuildOutputFn>,
    ssh_auth_sock: Option<&Path>,
) -> Result<()> {
    let mut cmd = Command::new("docker");

    if let Some(sock) = ssh_auth_sock {
        cmd.env("SSH_AUTH_SOCK", sock);
    }

    let load_target: Option<(&Host, Option<&Path>, String)> = match mode {
        BuildxMode::Load {
            docker_host,
            docker_socket,
        } => {
            apply_docker_host(&mut cmd, docker_host, *docker_socket);
            cmd.args(["buildx", "build", "--load"]);
            // Attestation manifests can collide with --load on some
            // buildx versions (e.g. macOS with default driver),
            // causing "image already exists" errors.
            cmd.args(["--provenance=false", "--sbom=false"]);
            let local_tag = format!("rumpelpod-{tag}");
            cmd.arg(format!("-t={local_tag}"));
            Some((docker_host, *docker_socket, local_tag))
        }
        BuildxMode::Push { registry, builder } => {
            cmd.args(["buildx", "build"]);
            if let Some(builder) = builder {
                cmd.args(["--builder", builder]);
            }
            cmd.args(["--push"]);
            cmd.args(["--provenance=false", "--sbom=false"]);
            let push_tag = format!("{registry}:{tag}");
            cmd.args(["-t", &push_tag]);
            None
        }
    };

    let dockerfile = dockerfile.display();
    cmd.arg(format!("-f={dockerfile}"));
    cmd.args(extra_args);
    cmd.arg(context.display().to_string());

    let build_result = if let Some(on_output) = on_output {
        run_and_stream(&mut cmd, on_output)
    } else {
        use crate::CommandExt;
        cmd.success().context("buildx build failed").map(|_| ())
    };

    // Tolerate the classic "image already exists" race: two
    // concurrent buildx --load builds targeting the same
    // deterministic tag will both export successfully, but only one
    // wins the final image import -- the other fails at the export
    // step even though the image is already present in the local
    // store.  Seen on macOS with the default docker driver.  If the
    // tag exists after the failure, the build effectively succeeded.
    if let Err(e) = &build_result {
        if let Some((docker_host, docker_socket, local_tag)) = load_target.as_ref() {
            let msg = format!("{e:#}");
            let is_race = msg.contains("already exists")
                && image_exists(local_tag, docker_host, *docker_socket);
            if is_race {
                return Ok(());
            }
        }
    }

    build_result
}

/// Spawn a docker command and stream its output through a callback.
///
/// Collects all output so it can be included in the error message on
/// failure.
fn run_and_stream(cmd: &mut Command, on_output: BuildOutputFn) -> Result<()> {
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    let child_stdout = child.stdout.take().expect("stdout was piped");
    let child_stderr = child.stderr.take().expect("stderr was piped");

    let callback = Arc::new(Mutex::new(on_output));
    let callback_for_stderr = callback.clone();

    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stdout_buf_clone = stdout_buf.clone();
    let stderr_buf_clone = stderr_buf.clone();

    let stdout_thread = std::thread::spawn(move || {
        for line in BufReader::new(child_stdout).lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            stdout_buf_clone.lock().unwrap().push_str(&line);
            stdout_buf_clone.lock().unwrap().push('\n');
            callback.lock().unwrap()(OutputLine::Stdout(line));
        }
    });

    let stderr_thread = std::thread::spawn(move || {
        for line in BufReader::new(child_stderr).lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            stderr_buf_clone.lock().unwrap().push_str(&line);
            stderr_buf_clone.lock().unwrap().push('\n');
            callback_for_stderr.lock().unwrap()(OutputLine::Stderr(line));
        }
    });

    let status = child.wait()?;
    stdout_thread.join().expect("stdout reader panicked");
    stderr_thread.join().expect("stderr reader panicked");

    if !status.success() {
        let stdout = stdout_buf.lock().unwrap();
        let stderr = stderr_buf.lock().unwrap();
        return Err(anyhow::anyhow!(
            "buildx build failed:\nSTDOUT: {stdout}\nSTDERR: {stderr}"
        ));
    }

    Ok(())
}

/// Pull a Docker image from its registry.
///
/// Inherits stdout/stderr so the user sees download progress.
pub fn pull_image(
    image_name: &str,
    docker_host: &Host,
    docker_socket: Option<&Path>,
) -> Result<()> {
    let mut cmd = Command::new("docker");
    apply_docker_host(&mut cmd, docker_host, docker_socket);
    cmd.args(["pull", image_name]);

    let status = cmd.status()?;
    if !status.success() {
        return Err(anyhow::anyhow!("docker pull failed with status {status}"));
    }
    Ok(())
}

/// Check whether a Docker image already exists in a remote registry.
///
/// Returns `Ok(true)` if the manifest exists, `Ok(false)` if the
/// registry reports "not found".  All other errors (unreachable
/// registry, auth failures) are propagated.
///
/// `--insecure` is required to reach plain-HTTP registries (k3d's
/// `registry.localhost` and equivalents); for HTTPS registries it
/// just relaxes cert verification, which is fine because the only
/// bit we trust here is the presence/absence signal.
pub(crate) fn registry_image_exists(registry_tag: &str) -> Result<bool> {
    let output = Command::new("docker")
        .args(["manifest", "inspect", "--insecure", registry_tag])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("running docker manifest inspect")?;

    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_lower = stderr.to_ascii_lowercase();

    if stderr_lower.contains("no such manifest")
        || stderr_lower.contains("manifest unknown")
        || stderr_lower.contains("not found")
    {
        return Ok(false);
    }

    Err(anyhow::anyhow!(
        "docker manifest inspect failed for '{registry_tag}':\n{stderr}"
    ))
}

/// Query the USER directive from a Docker image's configuration.
///
/// Tries a local `docker image inspect` first (free if the image
/// is cached on the target Docker host).  Falls back to querying
/// the OCI registry HTTP API, which downloads only the manifest
/// and config blob (a few KB), not the image layers.
pub(crate) fn inspect_image_user(
    image: &str,
    docker_host: &Host,
    docker_socket: Option<&Path>,
) -> Result<String> {
    // image_exists / docker inspect only work against a Docker daemon
    // (Localhost or SSH), not against a Kubernetes API.  For k8s we
    // go straight to the registry.
    let raw = if !matches!(docker_host, Host::Kubernetes { .. })
        && image_exists(image, docker_host, docker_socket)
    {
        let mut cmd = Command::new("docker");
        apply_docker_host(&mut cmd, docker_host, docker_socket);
        cmd.args(["image", "inspect", "--format", "{{.Config.User}}", image]);
        let output = cmd.output().context("inspecting image for USER")?;
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            crate::registry::fetch_image_user(image)?
        }
    } else {
        crate::registry::fetch_image_user(image)?
    };

    Ok(normalize_image_user(&raw))
}

/// Normalize the raw User string from an image config.
///
/// The image config stores whatever the Dockerfile's USER directive
/// contained, which may be a "user:group" pair or empty (meaning
/// root).  Normalize to a plain user (name or UID) suitable for
/// switch_user() and chown.
fn normalize_image_user(raw: &str) -> String {
    // Strip ":group" suffix (e.g. "1000:1000" -> "1000").
    let user = raw.split(':').next().unwrap_or(raw).trim();
    if user.is_empty() {
        "root".to_string()
    } else {
        user.to_string()
    }
}

/// Check whether a Docker image already exists on the target host.
pub(crate) fn image_exists(
    image_name: &str,
    docker_host: &Host,
    docker_socket: Option<&Path>,
) -> bool {
    let mut cmd = Command::new("docker");
    apply_docker_host(&mut cmd, docker_host, docker_socket);
    cmd.args(["image", "inspect", image_name]);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.status().is_ok_and(|s| s.success())
}

/// Compute a deterministic image tag based on the build configuration.
///
/// `path_tagging` decides whether the `context_path` location feeds the
/// hash.  For the built-in default image (`Ignore`) it must not: that
/// context is a tempdir that moves on every invocation, so hashing it
/// would defeat the cache.  For real contexts (`Include`) it does, so
/// two byte-identical contexts at different paths resolve to distinct
/// images rather than sharing one cached image -- the only part of the
/// hash distinguishing them, since `hash_context_dir` skips `.git`.
fn compute_image_tag(
    build: &BuildOptions,
    dockerfile_path: &Path,
    context_path: &Path,
    path_tagging: ContextPathTagging,
) -> Result<String> {
    let mut f = File::open(dockerfile_path)?;
    let mut content = Vec::new();
    f.read_to_end(&mut content)?;

    let mut hasher = Sha256::new();
    hasher.update(&content);
    match path_tagging {
        ContextPathTagging::Include => {
            hasher.update(context_path.as_os_str().as_encoded_bytes());
        }
        ContextPathTagging::Ignore => {}
    }
    hash_context_dir(&mut hasher, context_path)
        .with_context(|| format!("hashing build context at {}", context_path.display()))?;

    if let Some(args) = &build.args {
        let mut keys: Vec<&String> = args.keys().collect();
        keys.sort();
        for k in keys {
            hasher.update(k.as_bytes());
            hasher.update(args[k].as_bytes());
        }
    }

    if let Some(target) = &build.target {
        hasher.update(target.as_bytes());
    }

    if let Some(options) = &build.options {
        for opt in options {
            hasher.update(opt.as_bytes());
        }
    }

    if let Some(cache_from) = &build.cache_from {
        match cache_from {
            crate::devcontainer::StringOrArray::String(s) => hasher.update(s.as_bytes()),
            crate::devcontainer::StringOrArray::Array(arr) => {
                for s in arr {
                    hasher.update(s.as_bytes());
                }
            }
        }
    }

    let hash = hex::encode(&hasher.finalize()[..8]);
    Ok(hash)
}

/// Walk `context_path` and feed every entry into `hasher`.
///
/// The walk is sorted by file name so the resulting hash is
/// deterministic across filesystems.  Only the path *relative* to
/// `context_path` is hashed (plus the content or symlink target);
/// the absolute location of the context directory is intentionally
/// omitted so two identical contexts at different locations share
/// an image tag.  Timestamps are not hashed either -- Docker's
/// COPY layer cache works the same way.
///
/// `.git/` at the context root is skipped.  Rumpelpod itself writes
/// into `.git/` during `enter` (refs, hooks, submodule markers), so
/// including it would invalidate the image cache on every run for
/// the typical setup where the context is the repo root.  Projects
/// that need `.git/` baked into the image cannot rely on rumpelpod
/// caching anyway.
fn hash_context_dir(hasher: &mut Sha256, context_path: &Path) -> Result<()> {
    let walker = WalkDir::new(context_path)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !(e.depth() == 1 && e.file_name() == std::ffi::OsStr::new(".git")));
    for entry in walker {
        let entry = entry.context("walking build context")?;
        let rel_path = entry
            .path()
            .strip_prefix(context_path)
            .expect("walked path must live under context root");
        // Skip the context root itself: its relative path is empty
        // and its name is precisely the thing we don't want in the
        // hash.
        if rel_path.as_os_str().is_empty() {
            continue;
        }

        let rel_bytes = rel_path.to_string_lossy();
        hasher.update((rel_bytes.len() as u64).to_le_bytes());
        hasher.update(rel_bytes.as_bytes());

        let file_type = entry.file_type();
        if file_type.is_file() {
            hasher.update(b"f");
            let metadata = entry.metadata().context("stat entry")?;
            // Mode bits affect what COPY puts into the image
            // (notably the executable bit), so a chmod must bust
            // the cache even when file contents are unchanged.
            hasher.update((metadata.permissions().mode() as u64).to_le_bytes());
            let content = std::fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            hasher.update((content.len() as u64).to_le_bytes());
            hasher.update(&content);
        } else if file_type.is_dir() {
            hasher.update(b"d");
        } else if file_type.is_symlink() {
            hasher.update(b"l");
            let target = std::fs::read_link(entry.path())
                .with_context(|| format!("reading symlink {}", entry.path().display()))?;
            let target_bytes = target.to_string_lossy();
            hasher.update((target_bytes.len() as u64).to_le_bytes());
            hasher.update(target_bytes.as_bytes());
        } else {
            return Err(anyhow::anyhow!(
                "unsupported file type in build context: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}
