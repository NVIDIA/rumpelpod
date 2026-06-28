// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Build the rumpelhub container image at install time by staging
//! a tiny alpine Dockerfile plus the currently running `rumpel`
//! binary into a temp directory and pushing it to the configured
//! cluster registry under a content-addressed tag.  See
//! `notes/rumpelhub.md` ("Image") for the rationale.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Host;
use crate::image::{build_image_direct, BuildxMode};

/// Alpine-based Dockerfile used for the rumpelhub image.  Pulls
/// in `git` plus `git-daemon` (which is where Alpine ships
/// `git-http-backend`, the CGI binary the next slice will use to
/// serve the host repo to pods) and CA certs; the rest lives in
/// the `rumpel` binary copied in below.
const HUB_DOCKERFILE: &str = "\
FROM alpine:3.23.3
RUN apk add --no-cache git git-daemon ca-certificates
COPY rumpel /usr/local/bin/rumpel
ENTRYPOINT [\"/usr/local/bin/rumpel\"]
";

/// Stage the build context (Dockerfile + rumpel binary) into `dir`.
fn stage_build_context(dir: &Path) -> Result<()> {
    std::fs::write(dir.join("Dockerfile"), HUB_DOCKERFILE).context("writing staged Dockerfile")?;

    let rumpel_src = std::env::current_exe().context("resolving the running rumpel binary path")?;
    let rumpel_dst = dir.join("rumpel");
    std::fs::copy(&rumpel_src, &rumpel_dst).with_context(|| {
        format!(
            "copying {} -> {}",
            rumpel_src.display(),
            rumpel_dst.display()
        )
    })?;
    std::fs::set_permissions(&rumpel_dst, std::fs::Permissions::from_mode(0o755))
        .context("chmod +x staged rumpel binary")?;

    Ok(())
}

/// Content-addressed `rumpelhub-<commit>[-<dirty hash>]` tag.
/// Reuses the rumpel build's own version stamp from `build.rs`
/// instead of re-hashing the binary bytes -- the `+` separator
/// becomes `-` because `+` is not a legal Docker tag character.
/// The `rumpelhub-` prefix keeps these tags from colliding with
/// bare-hash devcontainer image tags in the same registry.
fn compute_hub_tag() -> String {
    let token = env!("RUMPELPOD_VERSION_INFO")
        .split_whitespace()
        .next_back()
        .unwrap_or("unknown");
    format!("rumpelhub-{}", token.replace('+', "-"))
}

/// Build the hub image and return its registry-qualified tag.
pub fn build_hub_image(host: &Host) -> Result<String> {
    let (registry, builder, engine) = match host {
        Host::Kubernetes {
            registry,
            builder,
            image_builder,
            ..
        } => (registry.to_string(), builder.clone(), *image_builder),
        Host::Localhost { .. } | Host::Ssh { .. } => {
            return Err(anyhow::anyhow!(
                "rumpel hub install only runs against Kubernetes hosts"
            ));
        }
    };

    let tag = compute_hub_tag();
    let staging = tempfile::tempdir().context("creating hub image build context dir")?;
    stage_build_context(staging.path())?;

    let mode = BuildxMode::Push {
        registry: registry.as_str(),
        builder: builder.as_deref(),
        engine,
    };
    let dockerfile = staging.path().join("Dockerfile");
    let context = staging.path();
    build_image_direct(&tag, &dockerfile, context, &mode).context("building rumpelhub image")
}
