// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Typed HTTP client for the in-container server.
//!
//! Startup awaits requests directly so task cancellation releases network
//! operations. Synchronous wrappers serve CLI and background callers.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use flate2::read::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::StreamReader;

use super::types::*;
use crate::async_runtime::block_on;
use crate::jitter;
use crate::RetryPolicy;

#[derive(Debug, Serialize, Deserialize)]
struct ErrorResponse {
    error: String,
}

pub struct PodClient {
    client: reqwest::Client,
    url: String,
    token: String,
}

impl PodClient {
    /// Synchronous entry point for CLI callers waiting on a pod.
    pub fn new(url: &str, token: &str, policy: RetryPolicy) -> Result<Self> {
        block_on(Self::new_async(url, token, policy))
    }

    pub async fn new_async(url: &str, token: &str, policy: RetryPolicy) -> Result<Self> {
        Self::wait_and_connect(url, token, move |msg| match policy {
            RetryPolicy::UserBlocking => eprintln!("{msg}"),
            RetryPolicy::Background => {}
        })
        .await
    }

    pub fn connect(url: &str, token: &str) -> Result<Self> {
        Self::new(url, token, RetryPolicy::UserBlocking)
    }

    pub fn new_with_timeout(url: &str, token: &str, timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .gzip(true)
                .build()?,
            url: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    /// Startup owns this future, so cancelling startup drops the request too.
    pub async fn wait_and_connect(
        url: &str,
        token: &str,
        on_progress: impl Fn(&str) + Sync,
    ) -> Result<Self> {
        let pod = Self {
            client: reqwest::Client::builder().gzip(true).build()?,
            url: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        };
        pod.wait_ready_async(Some(&on_progress)).await?;
        Ok(pod)
    }

    async fn wait_ready_async(&self, on_progress: Option<&(dyn Fn(&str) + Sync)>) -> Result<()> {
        let url = &self.url;
        let token = &self.token;
        // Startup heartbeats keep long lifecycle commands alive. A read
        // deadline catches broken transports without timing out healthy setup.
        let poll_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build poll client");

        let connect = async {
            let mut delay = Duration::from_millis(100);
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match poll_client
                    .get(format!("{url}/events"))
                    .header("Authorization", format!("Bearer {token}"))
                    .send()
                    .await
                {
                    Ok(resp) => return resp.error_for_status().context("opening readiness stream"),
                    Err(e) => {
                        if let Some(cb) = on_progress {
                            cb(&format!(
                                "waiting for container server (attempt {attempt}: {e})..."
                            ));
                        }
                    }
                }
                tokio::time::sleep(jitter(delay)).await;
                delay = delay.saturating_mul(2).min(Duration::from_secs(5));
            }
        };
        let response = tokio::time::timeout(Duration::from_secs(30), connect)
            .await
            .with_context(|| {
                format!("container server at {url} did not respond within 30 seconds")
            })??;

        match read_greeting(response, on_progress).await? {
            Some(lifecycle_err) => Err(anyhow::anyhow!("{lifecycle_err}")),
            None => Ok(()),
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
        crate::async_runtime::block_on(self.write_home_files_async(files, tar_extracts))
    }

    pub async fn write_home_files_async(
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
        .await
    }

    // -------------------------------------------------------------------
    // Filesystem (used by agents for ad-hoc file operations)
    // -------------------------------------------------------------------

    pub fn fs_read(&self, path: &Path) -> Result<Vec<u8>> {
        crate::async_runtime::block_on(self.fs_read_async(path))
    }

    pub async fn fs_read_async(&self, path: &Path) -> Result<Vec<u8>> {
        let resp: FsReadResponse = self
            .post(
                "/fs/read",
                &FsReadRequest {
                    path: path.to_path_buf(),
                },
            )
            .await?;
        base64_decode(&resp.content)
    }

    // -------------------------------------------------------------------
    // Git (patch transfer for dirty working tree)
    // -------------------------------------------------------------------

    /// GET /git/patch -- dirty-tree patch as raw bytes.  Empty Vec means
    /// the working tree is clean.
    pub async fn git_patch_get_async(&self) -> Result<Vec<u8>> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/git/patch");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if !response.status().is_success() {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            return Err(anyhow::anyhow!("GET /git/patch: {err}"));
        }

        let bytes = response.bytes().await.context("reading /git/patch body")?;
        Ok(bytes.to_vec())
    }

    /// GET /agent-files/<agent> -- streaming tar response (CompressionLayer
    /// applies transport gzip transparently).  Returns `Ok(None)` if
    /// the agent has no state to transfer (HTTP 404).
    pub async fn get_agent_files_async(&self, agent: &str) -> Result<Option<reqwest::Response>> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/agent-files/{agent}");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
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
        crate::async_runtime::block_on(self.put_agent_files_async(agent, reader, permission_hook))
    }

    pub async fn put_agent_files_async(
        &self,
        agent: &str,
        reader: impl std::io::Read + Send + 'static,
        permission_hook: Option<bool>,
    ) -> Result<()> {
        let gz_reader = GzEncoder::new(reader, Compression::fast());
        let body = reader_body(gz_reader);

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
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
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
    pub async fn get_container_env_async(
        &self,
    ) -> Result<std::collections::HashMap<String, String>> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/container-env");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if !response.status().is_success() {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            return Err(anyhow::anyhow!("GET /container-env: {err}"));
        }

        response
            .json()
            .await
            .context("parsing /container-env response")
    }

    /// GET /state -- pod metadata used by `rumpel fork`.
    pub fn get_state(&self) -> Result<StateResponse> {
        crate::async_runtime::block_on(self.get_state_async())
    }

    pub async fn get_state_async(&self) -> Result<StateResponse> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/state");
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if !response.status().is_success() {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            return Err(anyhow::anyhow!("GET /state: {err}"));
        }

        response.json().await.context("parsing /state response")
    }

    /// POST /git/push -- push every local branch to the rumpelpod
    /// remote, so a fresh fork can fetch them via `host`.
    pub fn git_push(&self) -> Result<()> {
        crate::async_runtime::block_on(self.git_push_async())
    }

    pub async fn git_push_async(&self) -> Result<()> {
        let base = &self.url;
        let token = &self.token;
        let url = format!("{base}/git/push");
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            Err(anyhow::anyhow!("POST /git/push: {err}"))
        }
    }

    /// POST /git/patch -- apply a patch produced by GET /git/patch.
    pub async fn git_patch_apply_async(&self, patch: &[u8]) -> Result<()> {
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
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
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
        crate::async_runtime::block_on(self.cp_download_async(path, follow_symlinks))
            .map(BlockingResponse::new)
    }

    pub async fn cp_download_async(
        &self,
        path: &Path,
        follow_symlinks: bool,
    ) -> Result<reqwest::Response> {
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
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if !response.status().is_success() {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
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
        crate::async_runtime::block_on(self.cp_upload_async(path, reader))
    }

    pub async fn cp_upload_async(
        &self,
        path: &Path,
        reader: impl std::io::Read + Send + 'static,
    ) -> Result<()> {
        let gz_reader = GzEncoder::new(reader, Compression::fast());
        let body = reader_body(gz_reader);

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
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
                error: "unknown error".to_string(),
            });
            let err = &error.error;
            Err(anyhow::anyhow!("POST /cp: {err}"))
        }
    }

    /// Populate bind mount targets inside the container from a single tar
    /// archive whose entries use absolute destination paths (leading slash
    /// stripped).  The reader is gzip-compressed on-the-fly and streamed.
    pub async fn init_mounts_async(
        &self,
        reader: impl std::io::Read + Send + 'static,
    ) -> Result<()> {
        let gz_reader = GzEncoder::new(reader, Compression::fast());
        let body = reader_body(gz_reader);

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
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
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
        crate::async_runtime::block_on(self.run_async(cmd, workdir, env, stdin, timeout_secs))
    }

    pub async fn run_async(
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
        .await
    }

    // -------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------

    async fn post<Req: Serialize, Resp: serde::de::DeserializeOwned>(
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
            .await
            .with_context(|| format!("sending request to {url}"))?;

        if response.status().is_success() {
            response
                .json()
                .await
                .with_context(|| format!("parsing response from {path}"))
        } else {
            let error: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
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
async fn read_greeting(
    resp: reqwest::Response,
    on_progress: Option<&(dyn Fn(&str) + Sync)>,
) -> Result<Option<String>> {
    let stream = futures_util::stream::try_unfold(resp, |mut resp| async move {
        let chunk = resp.chunk().await.map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(chunk.map(|bytes| (bytes, resp)))
    });
    let mut reader = BufReader::new(StreamReader::new(Box::pin(stream)));
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading event stream")?;
        if n == 0 {
            return Err(anyhow::anyhow!("event stream closed before state event"));
        }
        let trimmed = line.trim();
        if trimmed == "event: state" {
            let mut data_line = String::new();
            reader
                .read_line(&mut data_line)
                .await
                .context("reading state event data")?;
            // Consume blank separator line.
            let mut blank = String::new();
            reader
                .read_line(&mut blank)
                .await
                .context("reading state event separator")?;

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
                .await
                .context("reading progress event data")?;
            // Consume blank separator line.
            let mut blank = String::new();
            reader
                .read_line(&mut blank)
                .await
                .context("reading progress event separator")?;

            if let Some(cb) = on_progress {
                if let Some(msg) = data_line.trim().strip_prefix("data: ") {
                    cb(msg);
                }
            }
        }
    }
}

// CLI readers remain synchronous while startup awaits response bodies directly.
struct BlockingResponse {
    response: reqwest::Response,
    pending: std::io::Cursor<Vec<u8>>,
}

impl BlockingResponse {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            pending: std::io::Cursor::new(Vec::new()),
        }
    }
}

impl std::io::Read for BlockingResponse {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.pending.read(buf)?;
            if n != 0 {
                return Ok(n);
            }
            match block_on(self.response.chunk()).map_err(std::io::Error::other)? {
                Some(chunk) => self.pending = std::io::Cursor::new(chunk.to_vec()),
                None => return Ok(0),
            }
        }
    }
}

fn reader_body(reader: impl std::io::Read + Send + 'static) -> reqwest::Body {
    let stream = futures_util::stream::try_unfold(reader, |mut reader| async move {
        let mut bytes = vec![0; 64 * 1024];
        let n = reader.read(&mut bytes)?;
        bytes.truncate(n);
        Ok::<_, std::io::Error>(if n == 0 { None } else { Some((bytes, reader)) })
    });
    reqwest::Body::wrap_stream(stream)
}
