#!/bin/sh
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Provision a local k3d cluster with a host-accessible registry.
#
# Uses k3d's built-in --registry-create so the registry is reachable
# from the host (via /etc/hosts, added below) and from k3d nodes
# (via containerd mirror config).
#
# Docker daemon config (insecure-registries, dns) lives in the
# Dockerfile, not here -- it's static and shouldn't need a daemon
# restart at provision time.
#
# Writes kubeconfig and rumpelpod.json to $CLOUD_DIR/k3d/.

set -eu

CLUSTER_NAME=rumpelpod
CLOUD_DIR="${CLOUD_DIR:-/workspaces/rumpelpod/cloud}"
K3D_DIR="$CLOUD_DIR/k3d"
REGISTRY_NAME=rumpelpod-registry.localhost
REGISTRY_PORT=5000

mkdir -p "$K3D_DIR"

# The .localhost TLD is supposed to resolve to loopback (RFC 6761).
# glibc relies on systemd-resolved for this, and rumpel is a
# statically linked musl binary that bypasses NSS entirely and reads
# /etc/hosts directly -- so we need an explicit entry here either way.
# Has to happen at runtime: when the devcontainer runs as a k8s pod,
# kubelet rewrites /etc/hosts at pod start, so image-baked entries
# don't survive.
if ! grep -qF "$REGISTRY_NAME" /etc/hosts; then
    echo "==> Adding $REGISTRY_NAME to /etc/hosts..."
    printf '127.0.0.1\t%s\n' "$REGISTRY_NAME" >> /etc/hosts
fi

# If the cluster already exists, delete it first so we get a clean
# state with a fresh kubeconfig.
if k3d cluster list -o json | grep -q "\"name\":\"$CLUSTER_NAME\""; then
    echo "==> Deleting existing k3d cluster..."
    k3d cluster delete "$CLUSTER_NAME"
fi

# The nested k3s service CIDR must not overlap the outer cluster's.
# When this devcontainer runs as a pod on an outer k8s cluster, the
# inherited /etc/resolv.conf points at the outer cluster DNS (k3s
# default 10.43.0.10). k3d nodes forward DNS there, but if the nested
# cluster also uses the default 10.43.0.0/16 its own kube-proxy claims
# that range inside the nodes and hijacks the forwarded queries (the
# nested CoreDNS has no endpoints yet), so image pulls fail and CoreDNS
# never starts. Moving the nested CIDR off 10.43.0.0/16 lets the
# inherited resolver work as-is, no public DNS override needed.
echo "==> Creating k3d cluster $CLUSTER_NAME..."
k3d cluster create "$CLUSTER_NAME" \
    --registry-create "$REGISTRY_NAME:0.0.0.0:$REGISTRY_PORT" \
    --k3s-arg "--disable=traefik@server:0" \
    --k3s-arg "--service-cidr=10.45.0.0/16@server:0" \
    --k3s-arg "--cluster-dns=10.45.0.10@server:0" \
    --no-lb

echo "==> Exporting kubeconfig..."
k3d kubeconfig get "$CLUSTER_NAME" > "$K3D_DIR/kubeconfig"

export KUBECONFIG="$K3D_DIR/kubeconfig"

# -- Write rumpelpod.json -----------------------------------------------------
# On the host, the .localhost suffix resolves via /etc/hosts (added
# above); inside the cluster k3d's containerd mirror config routes
# to the container IP.

cat > "$K3D_DIR/rumpelpod.json" << JSON
{
  "kubernetes": {
    "context": "k3d-$CLUSTER_NAME",
    "registry": "$REGISTRY_NAME:$REGISTRY_PORT/rumpelpod"
  }
}
JSON

# This script runs as root (systemd service). Hand ownership to the
# container user so cargo xtest can read config files without
# permission issues.
chown -R "${CONTAINER_USER:?}" "$K3D_DIR"

echo "==> k3d cluster ready."
