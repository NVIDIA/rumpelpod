// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! HTTP cache proxy for LLM API requests (Claude CLI, Codex, etc.).
//!
//! Runs inside the git HTTP server (on the local machine) and caches API
//! responses to disk so tests can replay them deterministically.
//! The pod server inside containers forwards requests here via the
//! existing exec tunnel, so this works for local Docker, remote
//! Docker, and Kubernetes alike.
//!
//! Gated on the `RUMPELPOD_TEST_LLM_OFFLINE` environment variable:
//! present = test mode (the value controls online vs offline behavior).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Shared state for the LLM cache proxy routes on the git HTTP server.
#[derive(Clone)]
pub struct LlmCacheProxyState {
    /// Base cache directory (e.g. `llm-cache/`). Each provider gets
    /// its own subdirectory (e.g. `claude-cli/`, `codex/`).
    pub cache_base_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Provider configuration
// ---------------------------------------------------------------------------

struct ProviderConfig {
    /// Subdirectory under cache_base_dir for this provider's cache.
    cache_subdir: &'static str,
    /// Upstream API base URL (e.g. "https://api.anthropic.com").
    upstream_base: &'static str,
    /// Environment variable holding the real API key.
    api_key_env: &'static str,
    /// Fields extracted from the request body for the cache key.
    cache_fields: &'static [&'static str],
}

/// How to inject the API key into the forwarded request.
enum AuthStyle {
    /// Anthropic: `x-api-key: <key>` header.
    XApiKey,
    /// OpenAI: `Authorization: Bearer <key>` header.
    BearerToken,
}

fn provider_config(provider: &str) -> Option<(ProviderConfig, AuthStyle)> {
    match provider {
        "anthropic" => Some((
            ProviderConfig {
                cache_subdir: "claude-cli",
                upstream_base: "https://api.anthropic.com",
                api_key_env: "ANTHROPIC_API_KEY",
                cache_fields: &["model", "messages", "stream"],
            },
            AuthStyle::XApiKey,
        )),
        "openai" => Some((
            ProviderConfig {
                cache_subdir: "codex",
                upstream_base: "https://api.openai.com",
                api_key_env: "OPENAI_API_KEY",
                // `model` is intentionally excluded: Codex picks the
                // default model from its own version, so keying on it
                // would invalidate the cache on every Codex upgrade.
                cache_fields: &["messages", "input", "stream"],
            },
            AuthStyle::BearerToken,
        )),
        // xAI's API is OpenAI-compatible (chat completions, Bearer auth).
        "xai" => Some((
            ProviderConfig {
                cache_subdir: "grok",
                upstream_base: "https://api.x.ai",
                api_key_env: "XAI_API_KEY",
                cache_fields: &["model", "messages", "input", "stream"],
            },
            AuthStyle::BearerToken,
        )),
        _ => None,
    }
}

/// Serializes cache-miss API forwarding so parallel tests that miss
/// on the same request don't all call the real API.
static API_FORWARD_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Cache key computation
// ---------------------------------------------------------------------------

/// Reduce Codex's `input` array to the turns that actually determine
/// the answer: genuine user messages, minus the `<environment_context>`
/// block Codex injects as a user turn.  That block carries the cwd,
/// workspace root path, timezone, and date, all of which vary by machine
/// and by day and would otherwise make the cache non-portable.
fn strip_openai_non_user_input(value: &mut serde_json::Value) {
    let Some(input) = value
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    input.retain(|item| {
        item.get("role").and_then(serde_json::Value::as_str) == Some("user")
            && !is_environment_context(item)
    });
}

/// True when a Codex input item is the injected `<environment_context>`
/// turn, identified by a text part that opens with that tag.
fn is_environment_context(item: &serde_json::Value) -> bool {
    let Some(content) = item.get("content").and_then(serde_json::Value::as_array) else {
        return false;
    };
    content.iter().any(|part| {
        part.get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.trim_start().starts_with("<environment_context>"))
    })
}

/// Drop everything but the user turns from a chat-completions `messages`
/// array.  grok injects a volatile system prompt (working directory,
/// date, available tools) that would otherwise make the cache key unique
/// per run; keying on the user turns alone keeps it stable.
fn strip_non_user_messages(value: &mut serde_json::Value) {
    let Some(messages) = value
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    messages.retain(|item| item.get("role").and_then(serde_json::Value::as_str) == Some("user"));
}

/// Extract only the fields that determine API response semantics.
/// Everything else (system prompts, tools, metadata, temperature, etc.)
/// is excluded so the cache survives CLI version changes.
fn extract_cache_fields(provider: &str, body: &[u8], fields: &[&str]) -> Vec<u8> {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };

    let mut key_fields = serde_json::Map::new();
    for &field in fields {
        if let Some(val) = json.get(field) {
            key_fields.insert(field.to_string(), val.clone());
        }
    }

    let mut key_value = serde_json::Value::Object(key_fields);
    if provider == "openai" {
        strip_openai_non_user_input(&mut key_value);
    }
    if provider == "xai" {
        strip_non_user_messages(&mut key_value);
    }

    serde_json::to_vec(&key_value).unwrap_or_else(|_| body.to_vec())
}

/// Replace volatile date fragments in the serialized cache fields so
/// the hash stays stable across runs.  Codex, Claude, and Grok each
/// inject the current date into request bodies using different text
/// forms; all of them get the real date from the OS clock, which would
/// otherwise drift the cache key day to day.
fn normalize_cache_fields(data: Vec<u8>) -> Vec<u8> {
    let data = replace_dates_between(data, b"<current_date>", b"</current_date>");
    let data = replace_dates_between(data, b"Today's date is ", b".");
    replace_dates_between(data, b"Today's date: ", b"\\n")
}

/// Replace every `YYYY-MM-DD` that sits between `prefix` and `suffix`
/// with a fixed placeholder.  Multiple occurrences matter on the
/// claude --resume path, where each historical turn carries its own
/// dated system-reminder block.
fn replace_dates_between(data: Vec<u8>, prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
    const DATE_LEN: usize = 10; // "YYYY-MM-DD"
    const FIXED: &[u8] = b"0000-00-00";

    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let Some(rel) = data[i..].windows(prefix.len()).position(|w| w == prefix) else {
            out.extend_from_slice(&data[i..]);
            break;
        };
        let date_start = i + rel + prefix.len();
        let tag_end = date_start + DATE_LEN;
        if data.len() < tag_end + suffix.len() || &data[tag_end..tag_end + suffix.len()] != suffix {
            // Not a date here; keep looking past this prefix occurrence.
            out.extend_from_slice(&data[i..date_start]);
            i = date_start;
            continue;
        }
        out.extend_from_slice(&data[i..date_start]);
        out.extend_from_slice(FIXED);
        i = tag_end;
    }
    out
}

fn compute_cache_key(
    provider: &str,
    method: &str,
    path_and_query: &str,
    body: &[u8],
    fields: &[&str],
) -> String {
    let cache_fields = normalize_cache_fields(extract_cache_fields(provider, body, fields));
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(b"\n");
    hasher.update(path_and_query.as_bytes());
    hasher.update(b"\n");
    hasher.update(&cache_fields);
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// On-disk cache format
// ---------------------------------------------------------------------------

/// First line of a cached response file (JSON), followed by a newline
/// and the raw response body.
#[derive(Serialize, Deserialize)]
struct ResponseMeta {
    status: u16,
    path: String,
    headers: BTreeMap<String, String>,
}

fn response_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("response")
}

fn request_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("request")
}

/// Read a cached response: JSON metadata line, newline, raw body.
fn cache_get(cache_dir: &Path, key: &str) -> Option<(ResponseMeta, Vec<u8>)> {
    let data = std::fs::read(response_dir(cache_dir).join(key)).ok()?;
    let nl = data.iter().position(|&b| b == b'\n')?;
    let meta: ResponseMeta = serde_json::from_slice(&data[..nl]).ok()?;
    Some((meta, data[nl + 1..].to_vec()))
}

/// Write response file (JSON metadata + newline + raw body) and
/// request file (pretty-printed JSON for debugging, gitignored).
fn cache_put(
    cache_dir: &Path,
    key: &str,
    meta: &ResponseMeta,
    response_body: &[u8],
    request_body: &[u8],
) {
    let resp_dir = response_dir(cache_dir);
    let resp_path = resp_dir.join(key);
    let mut resp_data = serde_json::to_string(meta).expect("serialize response meta");
    resp_data.push('\n');
    let resp_bytes = [resp_data.as_bytes(), response_body].concat();
    atomic_write(&resp_dir, &resp_path, &resp_bytes);

    let req_dir = request_dir(cache_dir);
    let req_path = req_dir.join(key);
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(request_body) {
        let pretty = serde_json::to_string_pretty(&json).unwrap_or_default();
        atomic_write(&req_dir, &req_path, pretty.as_bytes());
    } else {
        atomic_write(&req_dir, &req_path, request_body);
    }
}

fn atomic_write(dir: &Path, final_path: &Path, data: &[u8]) {
    let temp = tempfile::Builder::new()
        .prefix("cache-")
        .tempfile_in(dir)
        .expect("create temp file for llm cache");
    std::fs::write(temp.path(), data).expect("write temp llm cache file");
    temp.persist(final_path).expect("persist llm cache file");
}

fn build_response(meta: &ResponseMeta, body: Vec<u8>) -> Response {
    let status = StatusCode::from_u16(meta.status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder().status(status);
    for (name, value) in &meta.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder.body(Body::from(body)).unwrap()
}

fn is_offline_mode() -> bool {
    std::env::var("RUMPELPOD_TEST_LLM_OFFLINE")
        .map(|s| s == "1" || s.to_lowercase() == "true")
        .unwrap_or(false)
}

fn is_websocket_upgrade(headers: &axum::http::HeaderMap) -> bool {
    headers.contains_key(axum::http::header::UPGRADE)
}

fn should_forward_request_header(name: &str, is_upgrade: bool) -> bool {
    if name == "host"
        || name == "transfer-encoding"
        || name == "authorization"
        || name == "x-api-key"
    {
        return false;
    }

    if name == "connection" {
        return is_upgrade;
    }

    true
}

fn should_forward_response_header(name: &str, status: u16) -> bool {
    if name == "transfer-encoding" {
        return false;
    }

    if name == "connection" {
        return status == StatusCode::SWITCHING_PROTOCOLS.as_u16();
    }

    true
}

// ---------------------------------------------------------------------------
// Axum handler (runs on the git HTTP server, host side)
// ---------------------------------------------------------------------------

/// Handle an LLM cache proxy request on the git HTTP server.
///
/// Authenticates the caller via bearer token (same as git endpoints),
/// then serves from cache or forwards to the real API.
/// The `provider` path segment selects the upstream API and cache dir.
pub async fn handle_llm_cache_proxy(
    State(state): State<LlmCacheProxyState>,
    axum::extract::Path((provider, rest)): axum::extract::Path<(String, String)>,
    req: Request<Body>,
) -> Response {
    let (config, auth_style) = match provider_config(&provider) {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown LLM provider: {provider}"),
            )
                .into_response();
        }
    };

    // The /llm-cache-proxy/* route is mounted only when
    // RUMPELPOD_TEST_LLM_OFFLINE is set (see daemon::main), and the
    // daemon's git HTTP server binds to 127.0.0.1 only, so only
    // same-machine processes can reach this handler.  In-pod claude
    // requests arrive via the pod server forwarder; host-side test
    // code hits the proxy directly.  Neither case needs an auth gate
    // here.

    let method = req.method().to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    // Reconstruct the API path from the captured wildcard segment.
    let path_and_query = format!("/{rest}{query}");

    // Collect headers for forwarding (skip hop-by-hop, host, auth,
    // and provider-specific auth headers the proxy will replace).
    let is_upgrade = is_websocket_upgrade(req.headers());
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter(|(name, _)| should_forward_request_header(name.as_str(), is_upgrade))
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect();

    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("llm-cache-proxy: failed to read request body: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let cache_key = compute_cache_key(
        &provider,
        &method,
        &path_and_query,
        &body_bytes,
        config.cache_fields,
    );
    let cache_dir = state.cache_base_dir.join(config.cache_subdir);

    eprintln!("llm-cache-proxy[{provider}]: {method} {path_and_query} -> key {cache_key}");

    // Lock-free cache lookup (files are immutable once written)
    if let Some((meta, body)) = cache_get(&cache_dir, &cache_key) {
        eprintln!("llm-cache-proxy[{provider}]: cache hit {method} {path_and_query}");
        return build_response(&meta, body);
    }

    if is_offline_mode() {
        eprintln!(
            "llm-cache-proxy[{provider}]: cache miss in offline mode\n\
             key: {cache_key}\n\
             {method} {path_and_query}\n\
             Re-run with RUMPELPOD_TEST_LLM_OFFLINE=0 to populate the cache."
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "cache miss in offline mode. \
             Re-run with RUMPELPOD_TEST_LLM_OFFLINE=0 to populate the cache.",
        )
            .into_response();
    }

    let upstream_base = config.upstream_base.to_string();
    let api_key_env = config.api_key_env.to_string();

    // Forward to real API, serialized to prevent duplicate calls
    tokio::task::spawn_blocking(move || {
        let _guard = API_FORWARD_LOCK.lock().unwrap();

        // Double-check after acquiring lock
        if let Some((meta, body)) = cache_get(&cache_dir, &cache_key) {
            return build_response(&meta, body);
        }

        let real_api_key = std::env::var(&api_key_env).unwrap_or_else(|_| {
            panic!("{api_key_env} must be set when RUMPELPOD_TEST_LLM_OFFLINE=0")
        });

        let url = format!("{upstream_base}{path_and_query}");

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .expect("build forwarding reqwest client");

        let mut request = client.request(
            method.parse().expect("parse HTTP method for forwarding"),
            &url,
        );

        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }

        match auth_style {
            AuthStyle::XApiKey => {
                request = request.header("x-api-key", &real_api_key);
            }
            AuthStyle::BearerToken => {
                request = request.header("authorization", format!("Bearer {real_api_key}"));
            }
        }

        request = request.body(body_bytes.to_vec());

        eprintln!("llm-cache-proxy[{provider}]: forwarding {method} {path_and_query}");

        let response = request
            .send()
            .unwrap_or_else(|e| panic!("forward request to {provider} API: {e}"));
        let status = response.status().as_u16();
        let mut resp_headers = BTreeMap::new();
        for (name, value) in response.headers() {
            let n = name.as_str();
            if !should_forward_response_header(n, status) {
                continue;
            }
            if let Ok(v) = value.to_str() {
                resp_headers.insert(n.to_string(), v.to_string());
            }
        }
        let response_body = response.bytes().expect("read forwarded response body");

        let meta = ResponseMeta {
            status,
            path: path_and_query.clone(),
            headers: resp_headers,
        };
        cache_put(&cache_dir, &cache_key, &meta, &response_body, &body_bytes);

        eprintln!("llm-cache-proxy[{provider}]: cached response {status} {cache_key}");

        build_response(&meta, response_body.to_vec())
    })
    .await
    .expect("API forwarding task panicked")
}

#[cfg(test)]
mod tests {
    use super::normalize_cache_fields;

    #[test]
    fn normalizes_grok_user_info_date() {
        let normalized = normalize_cache_fields(
            br#"{"content":"<user_info>\nToday's date: 2026-06-24\n</user_info>"}"#.to_vec(),
        );

        assert_eq!(
            normalized,
            br#"{"content":"<user_info>\nToday's date: 0000-00-00\n</user_info>"}"#
        );
    }
}
