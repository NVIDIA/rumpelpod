// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared request/response types for the pod HTTP protocol.
//!
//! Used by both the in-container server (`pod::server`) and the
//! host-side client (`pod::client`).  Both sides are always the same
//! rumpel binary, so these types do not need serde defaults or any
//! other backwards-compatibility machinery.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::git::GitIdentity;

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct FsReadRequest {
    pub path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FsReadResponse {
    /// Base64-encoded file content.
    pub content: String,
}

// Git: GET /git/patch returns the dirty-tree patch as raw bytes; POST
// /git/patch accepts the same bytes and applies them. No request types.

// ---------------------------------------------------------------------------
// State (cheap snapshot for fork)
// ---------------------------------------------------------------------------

/// Pod-side metadata that drives a fork: which branches exist, which
/// agents have writable state, whether the working tree is dirty.
/// Pure read -- no side effects.
#[derive(Debug, Serialize, Deserialize)]
pub struct StateResponse {
    pub branches: Vec<BranchInfo>,
    /// The pod's primary branch (the value of `git config rumpelpod.pod-name`).
    pub primary: String,
    /// Whether ~/.claude or ~/.claude.json exists.
    pub has_claude_state: bool,
    /// Whether ~/.codex exists.
    pub has_codex_state: bool,
    /// Whether ~/.pi exists.
    pub has_pi_state: bool,
    /// Whether rumpelpod has copied host pi config into this pod.
    pub has_pi_config: bool,
    /// Whether ~/.grok exists.
    pub has_grok_state: bool,
    /// Working tree has uncommitted changes.
    pub dirty: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub sha: String,
    /// Upstream as `git config branch.<name>.remote/branch.<name>.merge`
    /// would produce, e.g. "host/master", "rumpelpod/foo@bar", or a
    /// local branch name. None when no upstream is configured.
    pub upstream: Option<String>,
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct UserInfoResponse {
    pub home: String,
    pub shell: String,
    pub uid: u32,
    pub gid: u32,
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub cmd: Vec<String>,
    pub workdir: Option<PathBuf>,
    pub env: Vec<String>,
    /// Base64-encoded stdin data.
    pub stdin: Option<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RunResponse {
    pub exit_code: i32,
    /// Base64-encoded stdout.
    pub stdout: String,
    /// Base64-encoded stderr.
    pub stderr: String,
    pub timed_out: bool,
}

// ---------------------------------------------------------------------------
// Copy (tar-based file transfer)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct CpDownloadRequest {
    pub path: PathBuf,
    pub follow_symlinks: bool,
}

// Download is GET /cp with query parameters from CpDownloadRequest,
// returning a streamed tar body. Upload is POST /cp with a tar body
// and X-Path header.

// ---------------------------------------------------------------------------
// Git init (passed to container-serve at startup for first-time setup)
// ---------------------------------------------------------------------------

/// Clone-time parameters, only used on first launch.  Serialized as
/// JSON for the `rumpel container-serve --git-setup-spec` flag and
/// turned into a `GitSetupRequest` inside the container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSetupParams {
    pub branches: Vec<super::git_setup::GitSetupBranch>,
    pub primary: String,
    pub extra_host_fetch: Vec<String>,
    pub git_identity: Option<GitIdentity>,
}

// ---------------------------------------------------------------------------
// Write home files
// ---------------------------------------------------------------------------

/// Write multiple files under the container user's home directory in one call.
#[derive(Debug, Serialize, Deserialize)]
pub struct WriteHomeFilesRequest {
    /// Files to write, with paths relative to the user's home directory.
    pub files: Vec<HomeFileEntry>,
    /// Optional tar archive to extract under the home directory.
    pub tar_extracts: Vec<TarExtractEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeFileEntry {
    /// Path relative to home (e.g. ".claude/settings.json").
    pub path: String,
    /// Base64-encoded file content.
    pub content: String,
    /// Create parent directories if they do not exist.
    pub create_parents: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TarExtractEntry {
    /// Directory relative to home where the tar should be extracted.
    pub dest: String,
    /// Base64-encoded tar data.
    pub data: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WriteHomeFilesResponse {
    /// The home directory that was written to.
    pub home: String,
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Claude Code session state
// ---------------------------------------------------------------------------

/// Observable state of a Claude Code session inside the pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeState {
    /// Claude is actively generating a response.
    Processing,
    /// Claude finished its turn and is waiting for user input.
    WaitingForInput,
    /// An API authentication error occurred.
    AuthError,
    /// The session has ended.
    Stopped,
}

/// Hook -> pod server: report a state change.
#[derive(Debug, Serialize, Deserialize)]
pub struct NotifyClaudeStateRequest {
    pub state: ClaudeState,
}

// ---------------------------------------------------------------------------
// Codex session state
// ---------------------------------------------------------------------------

/// Observable state of a Codex session inside the pod.
///
/// Derived from `thread/status/changed` notifications on the Codex
/// app-server WebSocket protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexState {
    /// A turn is running and the agent is actively working.
    Processing,
    /// The thread is loaded but no turn is running.
    Idle,
    /// The agent hit a system error.
    Error,
}

/// POST body for the /codex-state endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct NotifyCodexStateRequest {
    pub state: CodexState,
}

// ---------------------------------------------------------------------------
// Gateway tunnel
// ---------------------------------------------------------------------------

/// Pod-local notification that the gateway tunnel has a new base URL.
#[derive(Debug, Serialize, Deserialize)]
pub struct RefreshGatewayRequest {
    pub base_url: String,
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Per-command outcome reported by the pod server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleCommandResult {
    pub name: String,
    pub exit_code: i32,
    /// Base64-encoded stderr (only populated on failure).
    pub stderr: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LifecycleResponse {
    /// Results of foreground (blocking) commands.
    pub results: Vec<LifecycleCommandResult>,
    /// Names of commands spawned in the background.
    pub background: Vec<String>,
}

// ---------------------------------------------------------------------------
// Base64 helpers
// ---------------------------------------------------------------------------

pub fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("base64 decode")
}
