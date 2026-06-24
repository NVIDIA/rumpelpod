// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Handlers for `rumpel image build` and `rumpel image fetch`.

use anyhow::Result;

use crate::cli::{ImageBuildCommand, ImageFetchCommand};
use crate::config::Host;
use crate::enter::load_for_image_cmd;
use crate::git::get_repo_root;
use crate::image::{self, BuildFlags};

pub fn build(cmd: &ImageBuildCommand) -> Result<()> {
    let repo_root = get_repo_root()?;
    let (devcontainer, docker_host) = load_for_image_cmd(&repo_root, cmd.host_args.resolve()?)?;

    if devcontainer.build.is_none() {
        return Err(anyhow::anyhow!(
            "this devcontainer uses 'image', not 'build'.\n\
             Use 'rumpel image fetch' to pull a pre-built image."
        ));
    }

    let on_output: Option<image::BuildOutputFn> =
        Some(Box::new(|line: image::OutputLine| match line {
            image::OutputLine::Stdout(s) => println!("{s}"),
            image::OutputLine::Stderr(s) => eprintln!("{s}"),
        }));

    let flags = BuildFlags {
        no_cache: cmd.no_cache,
        pull: cmd.pull,
        force: true,
    };

    // The client process invokes `docker buildx build` directly here,
    // so its own `SSH_AUTH_SOCK` (if any) is inherited by default.
    let result = image::resolve_image(
        &devcontainer,
        &docker_host,
        &repo_root,
        &flags,
        // `rumpel image build` only runs for explicit `build`
        // devcontainers (checked above), never the default image, so
        // the context path always belongs in the tag.
        image::ContextPathTagging::Include,
        on_output,
        None,
        None,
    )?;

    let image = &result.image.0;
    if matches!(docker_host, Host::Kubernetes { .. }) {
        println!("image built and pushed: {image}");
    } else {
        println!("image built: {image}");
    }

    Ok(())
}

pub fn fetch(cmd: &ImageFetchCommand) -> Result<()> {
    let repo_root = get_repo_root()?;
    let (devcontainer, docker_host) = load_for_image_cmd(&repo_root, cmd.host_args.resolve()?)?;

    if devcontainer.build.is_some() {
        return Err(anyhow::anyhow!(
            "this devcontainer uses 'build', not 'image'.\n\
             Use 'rumpel image build' to build from the Dockerfile."
        ));
    }

    if matches!(docker_host, Host::Kubernetes { .. }) {
        return Err(anyhow::anyhow!(
            "'rumpel image fetch' is not supported for Kubernetes hosts.\n\
             Images are pulled directly by the cluster."
        ));
    }

    let image_name = devcontainer
        .image
        .as_deref()
        .expect("either image or build must be set");

    image::pull_image(image_name, &docker_host, None)?;
    println!("image pulled: {image_name}");

    Ok(())
}
