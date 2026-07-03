// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Typed HTTP client for the in-container server.
//!
//! Follows the `DaemonClient` pattern from `daemon::protocol`: wraps
//! `reqwest::blocking::Client`, one method per endpoint, returns `Result<T>`.
//! Callers see synchronous method calls.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use flate2::read::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use super::types::*;
use crate::jitter;
use crate::RetryPolicy;

#[derive(Debug, Serialize, Deserialize)]
struct ErrorResponse {
    error: String,
}

pub struct PodClient {
    client: reqwest::blocking::Client,
    url: String,
    token: String,
}

impl PodClient {
    /// Connect to the container server, waiting for it to become ready.
    ///
    /// `policy` controls whether the readiness poll retries indefinitely
    /// (`UserBlocking`) or gives up after a fixed number of attempts
    /// (`Background`).
    pub fn new(url: &str, token: &str, policy: RetryPolicy) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(None)
            .gzip(true)
            .build()
            .expect("failed to build reqwest client");
        let pod = Self {
            client,
            url: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        };
        pod.wait_ready(policy)?;
        Ok(pod)
    }

    /// Connect to the container server, printing progress to stderr.
    ///
    /// Convenience for CLI commands where a user is waiting.
    pub fn connect(url: &str, token: &str) -> Result<Self> {
        Self::new(url, token, RetryPolicy::UserBlocking)
    }

    /// Wait for readiness, forwarding progress and retry messages
    /// through a callback instead of eprintln.
    pub fn wait_and_connect(url: &str, token: &str, on_progress: impl Fn(&str)) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(None)
            .gzip(true)
            .build()
            .expect("failed to build reqwest client");
        let pod = Self {
            client,
            url: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        };
        pod.wait_ready_impl(RetryPolicy::UserBlocking, Some(&on_progress))?;
        Ok(pod)
    }

    /// Build a client for callers that already have a live endpoint
    /// and need bounded request time instead of readiness polling.
    pub fn new_with_timeout(url: &str, token: &str, timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .gzip(true)
            .build()
            .expect("failed to build reqwest client");
        Ok(Self {
            client,
            url: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    /// Block until the container server accepts connections on /events
    /// and sends its `state` greeting.
    ///
    /// Uses exponential backoff (100ms doubling up to 30s) so
    /// high-latency links (e.g. remote Docker over slow WiFi) and slow
    /// lifecycle commands get enough time without hammering the
    /// connection.
    ///
    /// `UserBlocking` retries indefinitely with progress on stderr.
    /// `Background` gives up after 20 attempts.
    ///
    /// Returns an error if the greeting carries a lifecycle failure.
    fn wait_ready(&self, policy: RetryPolicy) -> Result<()> {
        let eprintln_progress = |msg: &str| eprintln!("{msg}");
        let on_progress: Option<&dyn Fn(&str)> = if policy == RetryPolicy::UserBlocking {
            Some(&eprintln_progress)
        } else {
            None
        };
        self.wait_ready_impl(policy, on_progress)
    }

    fn wait_ready_impl(
        &self,
        policy: RetryPolicy,
        on_progress: Option<&dyn Fn(&str)>,
    ) -> Result<()> {
        let url = &self.url;
        let token = &self.token;
        // No total timeout -- the SSE stream is open-ended and we
        // only read until the first `state` event.
        let poll_client = reqwest::blocking::Client::builder()
            .timeout(None)
            .build()
            .expect("failed to build poll client");

        let max_delay = Duration::from_secs(30);
        let mut delay = Duration::from_millis(100);
        let mut attempt = 0u32;

        let emit = |msg: &str| {
            if let Some(cb) = on_progress {
                cb(msg);
            }
        };

        loop {
            attempt += 1;
            match poll_client
                .get(format!("{url}/events"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
            {
                Ok(resp) if resp.status().is_success() => match read_greeting(resp, on_progress) {
                    Ok(Some(lifecycle_err)) => {
                        return Err(anyhow::anyhow!("{lifecycle_err}"));
                    }
                    Ok(None) => {
                        return Ok(());
                    }
                    Err(e) => {
                        emit(&format!(
                            "waiting for container server (attempt {attempt}: {e})..."
                        ));
                    }
                },
                Ok(resp) => {
                    let status = resp.status();
                    emit(&format!(
                        "waiting for container server (attempt {attempt}, status {status})..."
                    ));
                }
                Err(e) => {
                    emit(&format!(
                        "waiting for container server (attempt {attempt}: {e})..."
                    ));
                }
            }
            if policy == RetryPolicy::Background && attempt >= 20 {
                return Err(anyhow::anyhow!(
                    "container server at {url} did not become ready"
                ));
            }
            std::thread::sleep(jitter(delay));
            delay = delay.saturating_mul(2).min(max_delay);
        }
    }

    // -------------------------------------------------------------------
    // Write-home-files
    // -------------------------------------------------------------------

    /// Write multiple files under the container user's home directory.
    /// Returns the home directory path.
    pub fn write_home_files(
        &self,
        files: Vec<HomeFileEntry>,
        tar_extracts: Vec<TarExtractEntry>,
    ) -> Result<WriteHomeFilesResponse> {
        self.post(
            "/write-home-files",
            &WriteHomeFilesRequest {
                files,
                tar_extracts,
            },
        )
    }

    // -------------------------------------------------------------------
    // Filesystem (used by agents for ad-hoc file operations)
    // -------------------------------------------------------------------

    pub fn fs_read(&self, path: &Path) -> Result<Vec<u8>> {
        let resp: FsReadResponse = self.post(
            "/fs/read",
            &FsReadRequest {
                path: path.to_path_buf(),
            },
        )?;
        base64_decode(&resp.content)
    }

    // -------------------------------------------------------------------
    // Git (patch transfer for dirty working tree)
    // -------------------------------------------------------------------

    /// GET /git/patch -- dirty-tree patch as raw bytes.  Empty Vec means
    /// the working tree is clean.
    pub fn git_patch_get(&self) -> Result<Vec<u8>> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/git/patch");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if !response.status().is_success() {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            return Err(anyhow::anyhow!("GET /git/patch: {err}"));
        }

        let bytes = response.bytes().context("reading /git/patch body")?;
        Ok(bytes.to_vec())
    }

    /// GET /agent-files/<agent> -- streaming tar response (CompressionLayer
    /// applies transport gzip transparently).  Returns `Ok(None)` if
    /// the agent has no state to transfer (HTTP 404).
    pub fn get_agent_files(&self, agent: &str) -> Result<Option<impl std::io::Read>> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/agent-files/{agent}");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            return Err(anyhow::anyhow!("GET /agent-files/{agent}: {err}"));
        }

        Ok(Some(response))
    }

    /// PUT /agent-files/<agent> -- stream a tar.gz body for extraction.
    /// `permission_hook` becomes the `?permission_hook=` query
    /// parameter, which only affects claude's PermissionRequest hook;
    /// the statusLine and notify-state hooks are always rewritten
    /// server-side regardless.  `None` preserves the PermissionRequest
    /// entry that the uploaded settings.json already contains.
    pub fn put_agent_files(
        &self,
        agent: &str,
        reader: impl std::io::Read + Send + 'static,
        permission_hook: Option<bool>,
    ) -> Result<()> {
        let gz_reader = GzEncoder::new(reader, Compression::fast());
        let body = reqwest::blocking::Body::new(gz_reader);

        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/agent-files/{agent}");
        let mut req = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/x-tar")
            .header("Content-Encoding", "gzip");
        if let Some(ph) = permission_hook {
            req = req.query(&[("permission_hook", if ph { "true" } else { "false" })]);
        }
        let response = req
            .body(body)
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            Err(anyhow::anyhow!("PUT /agent-files/{agent}: {err}"))
        }
    }

    /// GET /container-env -- snapshot of the daemon-configured
    /// `containerEnv` keys with their current process values.
    /// Used by `rumpel fork` to inherit env-file values from the
    /// running source pod rather than re-reading them from disk.
    pub fn get_container_env(&self) -> Result<std::collections::HashMap<String, String>> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/container-env");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if !response.status().is_success() {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            return Err(anyhow::anyhow!("GET /container-env: {err}"));
        }

        response.json().context("parsing /container-env response")
    }

    /// GET /state -- pod metadata used by `rumpel fork`.
    pub fn get_state(&self) -> Result<StateResponse> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/state");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if !response.status().is_success() {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            return Err(anyhow::anyhow!("GET /state: {err}"));
        }

        response.json().context("parsing /state response")
    }

    /// POST /git/push -- push every local branch to the rumpelpod
    /// remote, so a fresh fork can fetch them via `host`.
    pub fn git_push(&self) -> Result<()> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/git/push");
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            Err(anyhow::anyhow!("POST /git/push: {err}"))
        }
    }

    /// POST /git/patch -- apply a patch produced by GET /git/patch.
    pub fn git_patch_apply(&self, patch: &[u8]) -> Result<()> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/git/patch");
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/octet-stream")
            .body(patch.to_vec())
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            Err(anyhow::anyhow!("POST /git/patch: {err}"))
        }
    }

    // -------------------------------------------------------------------
    // Copy (tar-based file transfer)
    // -------------------------------------------------------------------

    /// Download a file or directory from the container as a tar stream.
    ///
    /// The archive uses the wrapper format (`_/<name>/...`) so the caller
    /// can distinguish files from directories by inspecting the tar entries.
    /// Returns a reader over the response body; data is streamed without
    /// buffering the entire archive in memory.
    pub fn cp_download(&self, path: &Path, follow_symlinks: bool) -> Result<impl std::io::Read> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/cp");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .query(&CpDownloadRequest {
                path: path.to_path_buf(),
                follow_symlinks,
            })
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if !response.status().is_success() {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            return Err(anyhow::anyhow!("GET /cp: {err}"));
        }

        Ok(response)
    }

    /// Upload a tar archive and extract it at `path` in the container.
    ///
    /// The archive must use the wrapper format (`_/<name>/...`).
    /// The reader is gzip-compressed on-the-fly and streamed as the
    /// request body; neither the tar nor the compressed data is buffered.
    pub fn cp_upload(
        &self,
        path: &Path,
        reader: impl std::io::Read + Send + 'static,
    ) -> Result<()> {
        let gz_reader = GzEncoder::new(reader, Compression::fast());
        let body = reqwest::blocking::Body::new(gz_reader);

        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/cp");
        let path_display = path.display();
        let req = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/x-tar")
            .header("Content-Encoding", "gzip")
            .header("X-Path", format!("{path_display}"));
        let response = req
            .body(body)
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            Err(anyhow::anyhow!("POST /cp: {err}"))
        }
    }

    /// Populate bind mount targets inside the container from a single tar
    /// archive whose entries use absolute destination paths (leading slash
    /// stripped).  The reader is gzip-compressed on-the-fly and streamed.
    pub fn init_mounts(&self, reader: impl std::io::Read + Send + 'static) -> Result<()> {
        let gz_reader = GzEncoder::new(reader, Compression::fast());
        let body = reqwest::blocking::Body::new(gz_reader);

        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/init-mounts");
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/x-tar")
            .header("Content-Encoding", "gzip")
            .body(body)
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            Err(anyhow::anyhow!("POST /init-mounts: {err}"))
        }
    }

    // -------------------------------------------------------------------
    // Command execution
    // -------------------------------------------------------------------

    pub fn run(
        &self,
        cmd: &[&str],
        workdir: Option<&Path>,
        env: &[String],
        stdin: Option<&[u8]>,
        timeout_secs: Option<u64>,
    ) -> Result<RunResponse> {
        self.post(
            "/run",
            &RunRequest {
                cmd: cmd.iter().map(|s| s.to_string()).collect(),
                workdir: workdir.map(|p| p.to_path_buf()),
                env: env.to_vec(),
                stdin: stdin.map(base64_encode),
                timeout_secs,
            },
        )
    }

    // -------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------

    fn post<Req: Serialize, Resp: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &Req,
    ) -> Result<Resp> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}{path}");
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(body)
            .send()
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            response
                .json()
                .with_context(|| format!("parsing response from {path}"))
        } else {
            let error: ErrorResponse = response.json().unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            Err(anyhow::anyhow!("{path}: {err}"))
        }
    }
}

/// Read SSE events until the `state` greeting arrives.
///
/// `event: progress` lines are forwarded to `on_progress` (if set) so
/// the CLI can display container-serve startup steps.
///
/// Returns `Ok(Some(msg))` if the greeting carries a lifecycle error,
/// `Ok(None)` on success.
fn read_greeting(
    resp: reqwest::blocking::Response,
    on_progress: Option<&dyn Fn(&str)>,
) -> Result<Option<String>> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(resp);
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .context("reading event stream")?;
        if n == 0 {
            return Err(anyhow::anyhow!("event stream closed before state event"));
        }
        let trimmed = line.trim();
        if trimmed == "event: state" {
            let mut data_line = String::new();
            reader
                .read_line(&mut data_line)
                .context("reading state event data")?;
            // Consume blank separator line.
            let mut blank = String::new();
            reader.read_line(&mut blank).ok();

            let json_str = data_line
                .trim()
                .strip_prefix("data: ")
                .ok_or_else(|| anyhow::anyhow!("malformed state event"))?;
            let obj: serde_json::Value =
                serde_json::from_str(json_str).context("parsing state event")?;

            return Ok(obj
                .get("lifecycle_error")
                .and_then(|v| v.as_str())
                .map(String::from));
        }
        if trimmed == "event: progress" {
            let mut data_line = String::new();
            reader
                .read_line(&mut data_line)
                .context("reading progress event data")?;
            // Consume blank separator line.
            let mut blank = String::new();
            reader.read_line(&mut blank).ok();

            if let Some(cb) = on_progress {
                if let Some(msg) = data_line.trim().strip_prefix("data: ") {
                    cb(msg);
                }
            }
        }
    }
}
