// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#[path = "cli/claude/mod.rs"]
mod claude;
#[path = "cli/codex/mod.rs"]
mod codex;
#[path = "cli/common.rs"]
mod common;
#[path = "cli/completions.rs"]
mod completions;
#[path = "cli/container_name.rs"]
mod container_name;
#[path = "cli/cp.rs"]
mod cp;
#[path = "cli/delete.rs"]
mod delete;
#[path = "cli/description_hook.rs"]
mod description_hook;
#[path = "cli/devcontainer/mod.rs"]
mod devcontainer;
#[path = "cli/enter.rs"]
mod enter;
#[path = "cli/env_probe_hang.rs"]
mod env_probe_hang;
#[path = "cli/executor.rs"]
mod executor;
#[path = "cli/fork.rs"]
mod fork;
#[path = "cli/gateway/mod.rs"]
mod gateway;
#[path = "cli/grok/mod.rs"]
mod grok;
#[path = "cli/k8s/mod.rs"]
mod k8s;
#[path = "cli/list.rs"]
mod list;
#[path = "cli/merge.rs"]
mod merge;
#[path = "cli/network.rs"]
mod network;
#[path = "cli/pi/mod.rs"]
mod pi;
#[path = "cli/podman.rs"]
mod podman;
#[path = "cli/prune.rs"]
mod prune;
#[path = "cli/recreate.rs"]
mod recreate;
#[path = "cli/review.rs"]
mod review;
#[path = "cli/setup_progress.rs"]
mod setup_progress;
#[path = "cli/ssh/mod.rs"]
mod ssh;
#[path = "cli/ssh_agent.rs"]
mod ssh_agent;
#[path = "cli/stop.rs"]
mod stop;
#[cfg(target_os = "linux")]
#[path = "cli/systemd.rs"]
mod systemd;
#[path = "cli/version.rs"]
mod version;
