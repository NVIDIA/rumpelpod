// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconnection events streamed from the daemon to attached clients.

use serde::{Deserialize, Serialize};

/// Events streamed to clients waiting for a pod reconnection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReconnectEvent {
    /// The daemon is attempting to reconnect (host or pod).
    Attempting,
    /// The host connection has been restored; now connecting to the pod.
    HostConnected,
    /// The pod event endpoint confirmed the connection.
    Connected,
    /// A reconnection attempt failed; the daemon will retry.
    Failed { error: String },
    /// The pod was intentionally stopped; the client should exit.
    Stopped,
}
