# syntax=docker/dockerfile:1.7

# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

ARG BASE_IMAGE=debian:trixie

FROM $BASE_IMAGE

USER root

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
        kubectl \
        screen \
        skopeo \
        procps \
        unzip

# Docker daemon config for the nested k3d setup:
# - insecure-registries: the k3d built-in registry speaks HTTP. The
#   .localhost suffix resolves to ::1 (IPv6), which falls outside
#   Docker's default insecure range (127.0.0.0/8).
# - dns: when the devcontainer runs as a pod on an outer k8s cluster,
#   /etc/resolv.conf points at a cluster service IP (10.x) that's not
#   reachable from Docker's bridge network namespace. Without explicit
#   DNS here, k3d nodes cannot resolve registry-1.docker.io and k3s
#   fails to pull the pause image, so CoreDNS never starts.
RUN mkdir -p /etc/docker && printf '%s\n' \
    '{' \
    '  "insecure-registries": ["rumpelpod-registry.localhost:5000"],' \
    '  "dns": ["8.8.8.8", "1.1.1.1"]' \
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
    curl -fsSL "https://github.com/eksctl-io/eksctl/releases/latest/download/eksctl_Linux_${dpkg_arch}.tar.gz" \
    | tar xz -C /usr/local/bin

RUN curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m).zip" \
        -o /tmp/awscliv2.zip && \
    unzip -q /tmp/awscliv2.zip -d /tmp && \
    /tmp/aws/install && \
    rm -rf /tmp/awscliv2.zip /tmp/aws

# ECR credential helper so docker/buildx can push to ECR without
# manual `docker login` (tokens are refreshed automatically via AWS creds).
RUN curl -fsSL "https://amazon-ecr-credential-helper-releases.s3.us-east-2.amazonaws.com/0.9.0/linux-amd64/docker-credential-ecr-login" \
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
    usermod -aG docker ${USER}

USER ${USER}

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/home/${USER}/.cargo/bin:${PATH}"
RUN rustup component add rust-analyzer \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# cargo-about generates ATTRIBUTION.md; the attribution integration test
# shells out to it on every pipeline run.
RUN cargo install cargo-about --locked

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

RUN curl -fsSL https://claude.ai/install.sh | bash

USER root

# Install Codex CLI from GitHub releases.
# musl build is static, so it runs on any base image.
RUN ARCH=$(uname -m) && \
    case "$ARCH" in \
        x86_64) CODEX_ARCH="x86_64-unknown-linux-musl" ;; \
        aarch64) CODEX_ARCH="aarch64-unknown-linux-musl" ;; \
    esac && \
    curl -fsSL "https://github.com/openai/codex/releases/latest/download/codex-$CODEX_ARCH.tar.gz" \
    | tar xz -C /tmp && \
    mv "/tmp/codex-$CODEX_ARCH" /usr/local/bin/codex && \
    chmod +x /usr/local/bin/codex

RUN mkdir /workspaces && chown -R ${USER}:${USER} /workspaces
USER ${USER}

RUN --mount=type=ssh,uid=${USER_ID},gid=${GROUP_ID} \
    GIT_SSH_COMMAND="ssh -o StrictHostKeyChecking=accept-new" \
    git clone https://github.com/nvidia/rumpelpod /workspaces/rumpelpod
WORKDIR /workspaces/rumpelpod

# Populate target/ so incremental builds are fast.
RUN cargo build --bin rumpel --target x86_64-unknown-linux-musl \
    && cargo build --bin rumpel --target aarch64-unknown-linux-musl \
    && cargo build --bin xtest --bin pipeline \
    && cargo test --no-run

USER root

# Pre-download container images so services start without network access.
RUN mkdir /preloaded-images \
    && skopeo copy docker://cgr.dev/chainguard/wolfi-base:latest docker-archive:/preloaded-images/wolfi-base.tar:cgr.dev/chainguard/wolfi-base:latest \
    && skopeo copy docker://docker.io/rancher/k3s:v1.31.5-k3s1 docker-archive:/preloaded-images/k3s.tar:rancher/k3s:v1.31.5-k3s1 \
    && skopeo copy docker://ghcr.io/k3d-io/k3d-tools:5.8.3 docker-archive:/preloaded-images/k3d-tools.tar:ghcr.io/k3d-io/k3d-tools:5.8.3 \
    && skopeo copy docker://ghcr.io/k3d-io/k3d-proxy:5.8.3 docker-archive:/preloaded-images/k3d-proxy.tar:ghcr.io/k3d-io/k3d-proxy:5.8.3 \
    && skopeo copy docker://docker.io/library/registry:2 docker-archive:/preloaded-images/registry-2.tar:registry:2 \
    && skopeo copy docker://docker.io/moby/buildkit:v0.21.1 docker-archive:/preloaded-images/buildkit.tar:moby/buildkit:v0.21.1

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
