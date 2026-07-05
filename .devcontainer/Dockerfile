# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

ARG BASE_IMAGE=debian:testing-20260623@sha256:1c79280f13304aedb1d88795f80fbc26d818f951e56adab23d365ea626187271

FROM $BASE_IMAGE

USER root

# Freeze apt on the snapshot.debian.org timestamp matching the base image
# build date so package versions do not drift between rebuilds. Bump
# APT_SNAPSHOT and BASE_IMAGE together. Snapshotted Release files keep the
# Valid-Until stamp from their build day, so apt's staleness check must be
# off once the pin is older than a week; GPG verification still applies.
# snapshot.debian.org throttles under load, hence the retries.
ARG APT_SNAPSHOT=20260623T000000Z
RUN sed -i "s|^URIs:|Snapshot: ${APT_SNAPSHOT}\nCheck-Valid-Until: no\nURIs:|" \
        /etc/apt/sources.list.d/debian.sources \
    && echo 'Acquire::Retries "3";' > /etc/apt/apt.conf.d/80-retries

RUN export DEBIAN_FRONTEND=noninteractive \
    && apt-get update \
    && apt-get install --yes \
        systemd \
        systemd-sysv \
        libsystemd0 \
        dbus \
        kmod \
        iptables \
        iproute2 \
        udev \
        build-essential \
        make \
        cmake \
        pkg-config \
        git \
        git-lfs \
        curl \
        wget \
        ca-certificates \
        sudo \
        locales \
        gnupg \
        docker.io \
        podman \
        kubectl \
        screen \
        skopeo \
        procps \
        unzip

# Rootless Podman cannot create its user namespace under this outer sandbox.
# Use Podman's rootful API service. The devcontainer runtime creates a
# dedicated volume at /var/lib/containers; without it, Podman's rootful storage
# stays on the outer overlay root and falls back to fuse-overlayfs, which cannot
# run without /dev/fuse. Host networking avoids /dev/net/tun during Dockerfile
# RUN steps.
RUN mkdir -p \
        /etc/containers \
        /etc/systemd/system/podman.socket.d \
    && printf '%s\n' \
        '[storage]' \
        'driver = "overlay"' \
        'runroot = "/run/rumpelpod-podman/storage"' \
        'graphroot = "/var/lib/containers/storage"' \
        > /etc/containers/storage.conf \
    && printf '%s\n' \
        '[containers]' \
        'netns = "host"' \
        > /etc/containers/containers.conf \
    && printf '%s\n' \
        '[Socket]' \
        'SocketGroup=docker' \
        'SocketMode=0660' \
        'DirectoryMode=0755' \
        > /etc/systemd/system/podman.socket.d/rumpelpod.conf \
    && systemctl enable podman.socket

# Docker daemon config for the nested k3d setup:
# - insecure-registries: the k3d built-in registry speaks HTTP. The
#   .localhost suffix resolves to ::1 (IPv6), which falls outside
#   Docker's default insecure range (127.0.0.0/8).
# No "dns" override: k3d nodes inherit the pod's resolver from
# /etc/resolv.conf, which already forwards to the real host upstreams.
# The collision that used to break this (the nested k3s service CIDR
# overlapping the outer cluster's, so the inherited resolver IP got
# hijacked inside the nodes) is avoided by moving the nested CIDR in
# provision-k3d.sh.
RUN mkdir -p /etc/docker && printf '%s\n' \
    '{' \
    '  "insecure-registries": ["rumpelpod-registry.localhost:5000"]' \
    '}' > /etc/docker/daemon.json

RUN dpkg_arch=$(dpkg --print-architecture) && \
    curl -fsSL "https://github.com/vitobotta/hetzner-k3s/releases/download/v2.4.6/hetzner-k3s-linux-${dpkg_arch}" \
        -o /usr/local/bin/hetzner-k3s && \
    chmod +x /usr/local/bin/hetzner-k3s

# hetzner-k3s 2.4.6 validates that `helm` is on PATH at create time
# (it uses the cluster-autoscaler helm chart for the autoscaler addon
# the provisioner enables).  No upstream Debian package, so pull the
# static tarball from the helm release page.
RUN dpkg_arch=$(dpkg --print-architecture) && \
    curl -fsSL "https://get.helm.sh/helm-v3.16.3-linux-${dpkg_arch}.tar.gz" \
        -o /tmp/helm.tar.gz && \
    tar xz -C /tmp -f /tmp/helm.tar.gz && \
    mv /tmp/linux-${dpkg_arch}/helm /usr/local/bin/helm && \
    chmod +x /usr/local/bin/helm && \
    rm -rf /tmp/helm.tar.gz /tmp/linux-${dpkg_arch}

RUN curl -fsSL "https://github.com/k3d-io/k3d/releases/download/v5.8.3/k3d-linux-$(dpkg --print-architecture)" \
        -o /usr/local/bin/k3d && \
    chmod +x /usr/local/bin/k3d

RUN dpkg_arch=$(dpkg --print-architecture) && \
    curl -fsSL "https://github.com/eksctl-io/eksctl/releases/download/v0.229.0/eksctl_Linux_${dpkg_arch}.tar.gz" \
    | tar xz -C /usr/local/bin

RUN curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m)-2.35.15.zip" \
        -o /tmp/awscliv2.zip && \
    unzip -q /tmp/awscliv2.zip -d /tmp && \
    /tmp/aws/install && \
    rm -rf /tmp/awscliv2.zip /tmp/aws

# ECR credential helper so docker/buildx can push to ECR without
# manual `docker login` (tokens are refreshed automatically via AWS creds).
RUN dpkg_arch=$(dpkg --print-architecture) && \
    curl -fsSL "https://amazon-ecr-credential-helper-releases.s3.us-east-2.amazonaws.com/0.9.0/linux-${dpkg_arch}/docker-credential-ecr-login" \
        -o /usr/local/bin/docker-credential-ecr-login && \
    chmod +x /usr/local/bin/docker-credential-ecr-login

# Cross-compiler for the non-native architecture.
RUN export DEBIAN_FRONTEND=noninteractive && \
    dpkg_arch=$(dpkg --print-architecture) && \
    if [ "$dpkg_arch" = "amd64" ]; then \
        apt-get install --yes gcc-aarch64-linux-gnu; \
    elif [ "$dpkg_arch" = "arm64" ]; then \
        apt-get install --yes gcc-x86-64-linux-gnu; \
    fi

# Build musl from source for both x86_64 and aarch64.
# Debian's musl-tools only provides musl for the native architecture.
# CROSS_COMPILE sets the toolchain prefix so musl finds the right gcc/ar/ranlib.
# Even for native builds we use the full triplet prefix (e.g. x86_64-linux-gnu-)
# because musl's configure overrides an empty CROSS_COMPILE with "${target}-".
# Auto wrapper detection depends on a glibc probe that can fail for cross
# compilers when only the runtime cross libc is installed.
RUN curl -fsSL https://musl.libc.org/releases/musl-1.2.5.tar.gz | tar xz -C /tmp && \
    for target_arch in x86_64 aarch64; do \
        if [ "$target_arch" = "x86_64" ]; then \
            cross=x86_64-linux-gnu-; \
        else \
            cross=aarch64-linux-gnu-; \
        fi && \
        mkdir /tmp/musl-build-$target_arch && cd /tmp/musl-build-$target_arch && \
        CROSS_COMPILE="$cross" /tmp/musl-1.2.5/configure \
            --prefix=/opt/musl/$target_arch --target=$target_arch --enable-wrapper=gcc && \
        make -j$(nproc) && make install && \
        cp /opt/musl/$target_arch/bin/musl-gcc /usr/local/bin/${target_arch}-linux-musl-gcc; \
    done && \
    rm -rf /tmp/musl-1.2.5 /tmp/musl-build-*

RUN echo "ReadKMsg=no" >> /etc/systemd/journald.conf \
    && systemctl mask \
        proc-sys-fs-binfmt_misc.automount \
        apparmor.service \
        tmp.mount \
        systemd-udevd.service \
        systemd-sysctl.service \
        systemd-udevd-kernel.socket \
        systemd-udevd-control.socket \
        systemd-modules-load.service \
        sys-kernel-debug.mount \
        sys-kernel-tracing.mount \
        sys-kernel-config.mount \
        e2scrub_reap.service \
        e2scrub_all.timer

STOPSIGNAL SIGRTMIN+3

ARG USER=user
ARG USER_ID=1000
ARG GROUP_ID=1000

ENV USER=${USER}
ENV USER_ID=${USER_ID}
ENV GROUP_ID=${GROUP_ID}

# Ensure the user and group exist, handling the case where the base image
# already provides them (possibly with a different uid/gid/shell).
RUN if ! getent group ${USER} > /dev/null 2>&1; then \
        groupadd -g ${GROUP_ID} ${USER} || groupadd ${USER}; \
    fi && \
    if ! id -u ${USER} > /dev/null 2>&1; then \
        useradd -m -u ${USER_ID} -g ${USER} ${USER}; \
    fi && \
    echo "${USER} ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers && \
    usermod -aG docker -s /bin/bash ${USER}

ENV CONTAINER_HOST=unix:///run/podman/podman.sock

USER ${USER}

# The toolchain version must match rust-toolchain.toml at the repo root.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
        --default-toolchain 1.96.1
ENV PATH="/home/${USER}/.cargo/bin:${PATH}"
RUN rustup component add rust-analyzer \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# cargo-about generates ATTRIBUTION.md; the attribution integration test
# shells out to it on every pipeline run.
RUN cargo install cargo-about --version 0.9.1 --locked --features cli

# Set default target to native architecture. Cargo merges this with the
# project's .cargo/config.toml which has per-target linker settings.
RUN dpkg_arch=$(dpkg --print-architecture) && \
    if [ "$dpkg_arch" = "amd64" ]; then \
        default_target=x86_64-unknown-linux-musl; \
    else \
        default_target=aarch64-unknown-linux-musl; \
    fi && \
    mkdir -p ~/.cargo && \
    printf '[build]\ntarget = "%s"\n' "$default_target" > ~/.cargo/config.toml

RUN curl -fsSL https://claude.ai/install.sh | bash -s 2.1.201

USER root

# Install Codex CLI from GitHub releases.
# musl build is static, so it runs on any base image.
RUN ARCH=$(uname -m) && \
    case "$ARCH" in \
        x86_64) CODEX_ARCH="x86_64-unknown-linux-musl" ;; \
        aarch64) CODEX_ARCH="aarch64-unknown-linux-musl" ;; \
    esac && \
    curl -fsSL "https://github.com/openai/codex/releases/download/rust-v0.142.5/codex-$CODEX_ARCH.tar.gz" \
    | tar xz -C /tmp && \
    mv "/tmp/codex-$CODEX_ARCH" /usr/local/bin/codex && \
    chmod +x /usr/local/bin/codex

# Install the xAI Grok CLI: a single statically-linked binary served
# per (version, platform), pinned for reproducibility.
RUN ARCH=$(uname -m) && \
    case "$ARCH" in \
        x86_64) GROK_ARCH="x86_64" ;; \
        aarch64) GROK_ARCH="aarch64" ;; \
    esac && \
    curl -fsSL "https://x.ai/cli/grok-0.2.60-linux-$GROK_ARCH" -o /usr/local/bin/grok && \
    chmod +x /usr/local/bin/grok

ARG PI_VERSION=0.79.10
ARG NODE_VERSION=22.23.1

# pi requires a newer Node than stable distro packages often provide.
# The tarball is verified against the checksum file of its release.
RUN ARCH=$(uname -m) && \
    case "$ARCH" in \
        x86_64) NODE_ARCH="x64" ;; \
        aarch64) NODE_ARCH="arm64" ;; \
        *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;; \
    esac && \
    NODE_BASE="https://nodejs.org/dist/v${NODE_VERSION}" && \
    NODE_TARBALL="node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.gz" && \
    curl -fsSL "$NODE_BASE/$NODE_TARBALL" -o "/tmp/$NODE_TARBALL" && \
    curl -fsSL "$NODE_BASE/SHASUMS256.txt" -o /tmp/SHASUMS256.txt && \
    cd /tmp && \
    grep " $NODE_TARBALL\$" SHASUMS256.txt | sha256sum -c - && \
    tar xzf "/tmp/$NODE_TARBALL" -C /opt && \
    NODE_DIR="/opt/${NODE_TARBALL%.tar.gz}" && \
    ln -sf "$NODE_DIR/bin/node" /usr/local/bin/node && \
    ln -sf "$NODE_DIR/bin/npm" /usr/local/bin/npm && \
    ln -sf "$NODE_DIR/bin/npx" /usr/local/bin/npx && \
    rm "/tmp/$NODE_TARBALL" /tmp/SHASUMS256.txt && \
    npm install -g --prefix /usr/local --ignore-scripts --no-fund --no-audit \
        --loglevel=error "@earendil-works/pi-coding-agent@${PI_VERSION}"

RUN mkdir /workspaces && chown -R ${USER}:${USER} /workspaces
USER ${USER}

# The clone only warms the cargo cache below; RUMPELPOD_REF lets a CI
# build pin the exact commit it is testing instead of current main.
ARG RUMPELPOD_REF=main
RUN git clone https://github.com/nvidia/rumpelpod /workspaces/rumpelpod \
    && git -C /workspaces/rumpelpod checkout --detach "${RUMPELPOD_REF}"
WORKDIR /workspaces/rumpelpod

COPY --chown=${USER}:${USER} --chmod=755 commit-msg /workspaces/rumpelpod/.git/hooks/commit-msg

# Populate target/ so incremental builds are fast.
RUN cargo build --bin rumpel --target x86_64-unknown-linux-musl \
    && cargo build --bin rumpel --target aarch64-unknown-linux-musl \
    && cargo build --bin xtest --bin pipeline \
    && cargo test --no-run

USER root

# Pre-download container images so services start without network access.
# Sources are pinned by digest because tags (especially :latest and :2)
# move; the docker-archive references keep the tags the tooling expects.
RUN mkdir /preloaded-images \
    && skopeo copy docker://cgr.dev/chainguard/wolfi-base@sha256:2f7a5c164eafbdbe46fe1d91bd1ab4c8cb5c2bdbd10641c3d61bd39962384cdb docker-archive:/preloaded-images/wolfi-base.tar:cgr.dev/chainguard/wolfi-base:latest \
    && skopeo copy docker://docker.io/rancher/k3s@sha256:53cf744fe2fabf140cee240d2db70d13a4f2d98f1a13c98f58f457f423096917 docker-archive:/preloaded-images/k3s.tar:rancher/k3s:v1.31.5-k3s1 \
    && skopeo copy docker://ghcr.io/k3d-io/k3d-tools@sha256:c5ceb2b9e8ff2cf55917b0bee8bc9c54fe1483a16366b734117e2cfac1417bed docker-archive:/preloaded-images/k3d-tools.tar:ghcr.io/k3d-io/k3d-tools:5.8.3 \
    && skopeo copy docker://ghcr.io/k3d-io/k3d-proxy@sha256:d43e6ffd27c216d0b21815c1226c8aeac2e07086ac5a988eb39bd7e308f6e370 docker-archive:/preloaded-images/k3d-proxy.tar:ghcr.io/k3d-io/k3d-proxy:5.8.3 \
    && skopeo copy docker://docker.io/library/registry@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373 docker-archive:/preloaded-images/registry-2.tar:registry:2 \
    && skopeo copy docker://docker.io/moby/buildkit@sha256:87afb62ed6a762bb65b85d53819f3b341fb74a36d1fc0a1153a64f367637bfda docker-archive:/preloaded-images/buildkit.tar:moby/buildkit:v0.21.1

RUN printf '[Unit]\nAfter=docker.service\nRequires=docker.service\n\n[Service]\nType=oneshot\nExecStart=/bin/sh -c '"'"'for f in /preloaded-images/*.tar; do /usr/bin/docker load -i "$f"; done'"'"'\n\n[Install]\nWantedBy=multi-user.target\n' \
    > /etc/systemd/system/preload-images.service \
    && systemctl enable preload-images.service

COPY provision-k3d.sh /usr/local/bin/provision-k3d.sh
RUN chmod +x /usr/local/bin/provision-k3d.sh

RUN printf '[Unit]\nDescription=Provision k3d cluster\nAfter=preload-images.service\nRequires=preload-images.service\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/provision-k3d.sh\nEnvironment=CLOUD_DIR=/workspaces/rumpelpod/cloud\nEnvironment=CONTAINER_USER=%s\n\n[Install]\nWantedBy=multi-user.target\n' "${USER}" \
    > /etc/systemd/system/provision-k3d.service \
    && systemctl enable provision-k3d.service

COPY entrypoint.sh /usr/local/bin/devcontainer-entrypoint.sh

ENV RUMPELPOD_TEST_SYSTEMD=1

ENTRYPOINT ["/usr/local/bin/devcontainer-entrypoint.sh"]
