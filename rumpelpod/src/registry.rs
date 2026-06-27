// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Lightweight OCI registry client for querying image metadata.
//!
//! Only fetches manifests and config blobs (a few KB each), never
//! image layers.  Used to inspect the base image's USER directive
//! without downloading the full image.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::blocking::Client;
use serde::Deserialize;

/// Parsed OCI image reference.
pub(crate) struct ImageRef {
    /// Registry hostname (e.g. "registry-1.docker.io", "ghcr.io").
    pub registry: String,
    /// Repository path (e.g. "library/ubuntu", "user/repo").
    pub repository: String,
    /// Tag or digest (e.g. "latest", "sha256:abc...").
    pub reference: String,
}

impl ImageRef {
    /// The registry hostname used as key in container auth files.
    ///
    /// Docker Hub stores credentials under "https://index.docker.io/v1/"
    /// regardless of which hostname the user specified.
    fn auth_config_key(&self) -> &str {
        if self.registry == "registry-1.docker.io" {
            "https://index.docker.io/v1/"
        } else {
            &self.registry
        }
    }
}

/// Parse a Docker image reference into its components.
///
/// Handles Docker Hub shorthand (e.g. "ubuntu:22.04" ->
/// registry-1.docker.io/library/ubuntu:22.04) and explicit
/// registry references (e.g. "ghcr.io/user/repo:tag").
pub(crate) fn parse_image_ref(image: &str) -> ImageRef {
    // Split off @sha256:... digest if present.
    let (name, reference) = if let Some((name, digest)) = image.split_once('@') {
        (name, digest.to_string())
    } else if let Some((name, tag)) = image.rsplit_once(':') {
        // Distinguish "registry:port/repo" from "repo:tag":
        // if the part after ':' contains '/', it is a port number
        // followed by a repository path, not a tag.
        if tag.contains('/') {
            (image, "latest".to_string())
        } else {
            (name, tag.to_string())
        }
    } else {
        (image, "latest".to_string())
    };

    // Split registry from repository.
    let (registry, repository) = if let Some((first, rest)) = name.split_once('/') {
        // If the first component looks like a hostname (contains '.'
        // or ':', or is "localhost"), treat it as the registry.
        if first.contains('.') || first.contains(':') || first == "localhost" {
            (first.to_string(), rest.to_string())
        } else {
            ("docker.io".to_string(), name.to_string())
        }
    } else {
        // Bare name like "ubuntu" -> docker.io/library/ubuntu
        ("docker.io".to_string(), format!("library/{name}"))
    };

    // Docker Hub's API lives at registry-1.docker.io.
    let registry = if registry == "docker.io" {
        "registry-1.docker.io".to_string()
    } else {
        registry
    };

    ImageRef {
        registry,
        repository,
        reference,
    }
}

/// Fetch the USER directive from an image's OCI config via the
/// registry HTTP API.
///
/// Downloads only the manifest and config blob (a few KB total).
pub(crate) fn fetch_image_user(image: &str) -> Result<String> {
    let image_ref = parse_image_ref(image);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let scheme = registry_scheme(&image_ref.registry);
    let cred = authenticate(&client, &image_ref, scheme)?;

    let manifest = fetch_manifest(&client, &image_ref, scheme, cred.as_ref())?;
    let config_digest =
        config_digest_from_manifest(&manifest, &client, &image_ref, scheme, cred.as_ref())?;
    let config = fetch_blob(&client, &image_ref, scheme, &config_digest, cred.as_ref())?;

    let user = config
        .get("config")
        .and_then(|c| c.get("User"))
        .and_then(|u| u.as_str())
        .unwrap_or("");

    Ok(user.to_string())
}

// ---- HTTP scheme detection -------------------------------------------------

/// Local/private registries typically run plain HTTP.
fn registry_scheme(registry: &str) -> &'static str {
    let host = registry.split(':').next().unwrap_or(registry);
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
    {
        "http"
    } else {
        "https"
    }
}

// ---- Authentication --------------------------------------------------------

/// Credential obtained from authenticating with a registry.
///
/// Bearer tokens are what Docker Hub / GHCR use (the OCI distribution
/// token flow).  Basic auth is what AWS ECR uses -- ECR responds to
/// the `/v2/` probe with `WWW-Authenticate: Basic` and then accepts
/// the Docker-config credentials directly on every subsequent
/// request.  Per-request setup is the same shape for both: we just
/// pick the right header.
pub(crate) enum Credential {
    Bearer(String),
    Basic(String, String),
}

/// Authenticate with the registry.
///
/// Probes `/v2/` for a 401 challenge.  On a `Bearer` challenge, runs
/// the OCI token exchange and returns the bearer token.  On a `Basic`
/// challenge, looks up credentials in the docker config (credHelpers
/// / credsStore / auths) and returns them for per-request basic auth.
fn authenticate(client: &Client, image_ref: &ImageRef, scheme: &str) -> Result<Option<Credential>> {
    let url = format!("{scheme}://{}/v2/", image_ref.registry);
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("probing registry at {url}"))?;

    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }

    let www_auth = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .context("registry returned 401 without WWW-Authenticate header")?
        .to_string();

    let challenge = parse_www_authenticate(&www_auth).context("parsing WWW-Authenticate header")?;

    match challenge {
        AuthChallenge::Bearer { realm, service } => {
            let scope = format!("repository:{}:pull", image_ref.repository);
            let mut req = client
                .get(&realm)
                .query(&[("scope", scope.as_str()), ("service", service.as_str())]);

            if let Some((user, pass)) = registry_credentials(image_ref.auth_config_key()) {
                req = req.basic_auth(&user, Some(&pass));
            }

            let token_resp: serde_json::Value = req
                .send()
                .context("requesting auth token")?
                .error_for_status()
                .context("auth token request failed")?
                .json()
                .context("parsing auth token response")?;

            let token = token_resp
                .get("token")
                .or_else(|| token_resp.get("access_token"))
                .and_then(|t| t.as_str())
                .context("no token in auth response")?;

            Ok(Some(Credential::Bearer(token.to_string())))
        }
        AuthChallenge::Basic => {
            let (user, pass) =
                registry_credentials(image_ref.auth_config_key()).with_context(|| {
                    let key = image_ref.auth_config_key();
                    format!(
                        "registry {} requires Basic auth but no credentials \
                         are configured for it in container auth files \
                         (checked auths, credHelpers, credsStore for key {key:?})",
                        image_ref.registry,
                    )
                })?;
            Ok(Some(Credential::Basic(user, pass)))
        }
    }
}

/// Parsed WWW-Authenticate challenge.
#[cfg_attr(test, derive(Debug))]
enum AuthChallenge {
    /// Bearer realm="...",service="..."
    Bearer { realm: String, service: String },
    /// Basic realm="...",service="..." -- realm/service are unused
    /// for Basic because there is no token endpoint to call.
    Basic,
}

/// Parse `WWW-Authenticate: <scheme> realm="...",service="..."` (plus
/// any extra comma-separated params we ignore).  Both `Bearer` (OCI
/// token flow) and `Basic` (ECR) are recognised.
fn parse_www_authenticate(header: &str) -> Result<AuthChallenge> {
    // Case-insensitive match on the scheme word.  The body of the
    // challenge is shared between the two schemes (realm/service),
    // so splitting on whitespace once is enough.
    let (scheme_word, rest) = header.split_once(' ').unwrap_or((header, ""));

    let mut realm = None;
    let mut service = None;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some((key, value)) = part.split_once('=') {
            let value = value.trim_matches('"');
            match key.trim() {
                "realm" => realm = Some(value.to_string()),
                "service" => service = Some(value.to_string()),
                _ => {}
            }
        }
    }

    if scheme_word.eq_ignore_ascii_case("Bearer") {
        Ok(AuthChallenge::Bearer {
            realm: realm.context("missing realm in Bearer challenge")?,
            service: service.unwrap_or_default(),
        })
    } else if scheme_word.eq_ignore_ascii_case("Basic") {
        Ok(AuthChallenge::Basic)
    } else {
        Err(anyhow::anyhow!(
            "unsupported auth scheme {scheme_word:?}; \
             expected Bearer or Basic"
        ))
    }
}

// ---- Container credential lookup -------------------------------------------

/// Read credentials for a registry from Docker and Podman auth files.
///
/// Checks `credHelpers`, `credsStore`, and direct `auths` entries.
fn registry_credentials(registry: &str) -> Option<(String, String)> {
    for config_path in auth_config_paths() {
        let Some(creds) = credentials_from_config(&config_path, registry) else {
            continue;
        };
        return Some(creds);
    }
    None
}

fn auth_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("REGISTRY_AUTH_FILE") {
        paths.push(PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".docker/config.json"));
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        paths.push(PathBuf::from(runtime_dir).join("containers/auth.json"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".config/containers/auth.json"));
    }
    paths
}

fn credentials_from_config(config_path: &PathBuf, registry: &str) -> Option<(String, String)> {
    let content = std::fs::read_to_string(config_path).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;

    // credHelpers: per-registry credential helper overrides.
    if let Some(helper) = config
        .get("credHelpers")
        .and_then(|h| h.get(registry))
        .and_then(|h| h.as_str())
    {
        if let Some(creds) = credentials_from_helper(helper, registry) {
            return Some(creds);
        }
    }

    // credsStore: default credential helper for all registries.
    if let Some(store) = config.get("credsStore").and_then(|s| s.as_str()) {
        if let Some(creds) = credentials_from_helper(store, registry) {
            return Some(creds);
        }
    }

    // auths: direct base64-encoded credentials.
    credentials_from_auths(&config, registry)
}

/// Shell out to `docker-credential-<helper>` to retrieve credentials.
fn credentials_from_helper(helper: &str, registry: &str) -> Option<(String, String)> {
    let helper_bin = format!("docker-credential-{helper}");
    let mut child = Command::new(&helper_bin)
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    child.stdin.take()?.write_all(registry.as_bytes()).ok()?;

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }

    #[derive(Deserialize)]
    struct CredentialResponse {
        #[serde(alias = "Username")]
        username: String,
        #[serde(alias = "Secret")]
        secret: String,
    }

    let creds: CredentialResponse = serde_json::from_slice(&output.stdout).ok()?;
    Some((creds.username, creds.secret))
}

/// Look up credentials in the `auths` section of the docker config.
fn credentials_from_auths(config: &serde_json::Value, registry: &str) -> Option<(String, String)> {
    let auths = config.get("auths")?;

    // Docker stores auth under various key formats depending on
    // how `docker login` was invoked.
    let candidates = [
        registry.to_string(),
        format!("https://{registry}"),
        format!("https://{registry}/v1/"),
        format!("https://{registry}/v2/"),
    ];

    for key in &candidates {
        let result = auths
            .get(key.as_str())
            .and_then(|a| a.get("auth"))
            .and_then(|a| a.as_str())
            .and_then(|auth| base64::engine::general_purpose::STANDARD.decode(auth).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|decoded| {
                let (user, pass) = decoded.split_once(':')?;
                Some((user.to_string(), pass.to_string()))
            });
        if result.is_some() {
            return result;
        }
    }

    None
}

// ---- Manifest / blob fetching ----------------------------------------------

const MANIFEST_ACCEPT: &str = "\
    application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json";

fn authed_get(
    client: &Client,
    url: &str,
    cred: Option<&Credential>,
) -> reqwest::blocking::RequestBuilder {
    let req = client.get(url);
    match cred {
        Some(Credential::Bearer(t)) => req.bearer_auth(t),
        Some(Credential::Basic(u, p)) => req.basic_auth(u, Some(p)),
        None => req,
    }
}

fn fetch_manifest(
    client: &Client,
    image_ref: &ImageRef,
    scheme: &str,
    cred: Option<&Credential>,
) -> Result<serde_json::Value> {
    let url = format!(
        "{scheme}://{}/v2/{}/manifests/{}",
        image_ref.registry, image_ref.repository, image_ref.reference
    );

    authed_get(client, &url, cred)
        .header("Accept", MANIFEST_ACCEPT)
        .send()
        .context("fetching manifest")?
        .error_for_status()
        .with_context(|| format!("fetching manifest for {}", image_ref.repository))?
        .json()
        .context("parsing manifest JSON")
}

/// Check whether an image manifest exists in a registry.
pub(crate) fn image_manifest_exists(image: &str) -> Result<bool> {
    let image_ref = parse_image_ref(image);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let scheme = registry_scheme(&image_ref.registry);
    let cred = authenticate(&client, &image_ref, scheme)?;
    let url = format!(
        "{scheme}://{}/v2/{}/manifests/{}",
        image_ref.registry, image_ref.repository, image_ref.reference
    );

    let resp = authed_get(&client, &url, cred.as_ref())
        .header("Accept", MANIFEST_ACCEPT)
        .send()
        .with_context(|| format!("fetching manifest for {image}"))?;

    if resp.status().is_success() {
        return Ok(true);
    }
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    let text_lower = text.to_ascii_lowercase();
    if text_lower.contains("manifest unknown") || text_lower.contains("not found") {
        return Ok(false);
    }

    Err(anyhow::anyhow!(
        "registry manifest lookup failed for '{image}' with status {status}: {text}"
    ))
}

/// Extract the config digest, following through manifest lists if
/// needed.  The USER is architecture-independent so any platform
/// entry works.
fn config_digest_from_manifest(
    manifest: &serde_json::Value,
    client: &Client,
    image_ref: &ImageRef,
    scheme: &str,
    cred: Option<&Credential>,
) -> Result<String> {
    let media_type = manifest
        .get("mediaType")
        .and_then(|m| m.as_str())
        .unwrap_or("");

    match media_type {
        "application/vnd.docker.distribution.manifest.v2+json"
        | "application/vnd.oci.image.manifest.v1+json" => manifest
            .get("config")
            .and_then(|c| c.get("digest"))
            .and_then(|d| d.as_str())
            .map(String::from)
            .context("manifest has no config digest"),

        "application/vnd.docker.distribution.manifest.list.v2+json"
        | "application/vnd.oci.image.index.v1+json" => {
            let manifests = manifest
                .get("manifests")
                .and_then(|m| m.as_array())
                .context("manifest list has no manifests array")?;

            let entry = manifests.first().context("manifest list is empty")?;

            let digest = entry
                .get("digest")
                .and_then(|d| d.as_str())
                .context("manifest entry has no digest")?;

            let platform_ref = ImageRef {
                registry: image_ref.registry.clone(),
                repository: image_ref.repository.clone(),
                reference: digest.to_string(),
            };
            let platform_manifest = fetch_manifest(client, &platform_ref, scheme, cred)?;
            config_digest_from_manifest(&platform_manifest, client, image_ref, scheme, cred)
        }

        _ => Err(anyhow::anyhow!(
            "unsupported manifest media type: {media_type}"
        )),
    }
}

fn fetch_blob(
    client: &Client,
    image_ref: &ImageRef,
    scheme: &str,
    digest: &str,
    cred: Option<&Credential>,
) -> Result<serde_json::Value> {
    let url = format!(
        "{scheme}://{}/v2/{}/blobs/{}",
        image_ref.registry, image_ref.repository, digest
    );

    authed_get(client, &url, cred)
        .send()
        .context("fetching config blob")?
        .error_for_status()
        .context("fetching config blob")?
        .json()
        .context("parsing config blob JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_name() {
        let r = parse_image_ref("ubuntu");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parse_name_with_tag() {
        let r = parse_image_ref("ubuntu:22.04");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.reference, "22.04");
    }

    #[test]
    fn parse_user_repo() {
        let r = parse_image_ref("user/repo:v1");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "user/repo");
        assert_eq!(r.reference, "v1");
    }

    #[test]
    fn parse_explicit_registry() {
        let r = parse_image_ref("ghcr.io/owner/repo:latest");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "owner/repo");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parse_registry_with_port() {
        let r = parse_image_ref("localhost:5000/myimage:v2");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "myimage");
        assert_eq!(r.reference, "v2");
    }

    #[test]
    fn parse_digest_reference() {
        let r = parse_image_ref("ubuntu@sha256:abcdef");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/ubuntu");
        assert_eq!(r.reference, "sha256:abcdef");
    }

    #[test]
    fn parse_registry_with_port_no_tag() {
        let r = parse_image_ref("myregistry:5000/repo/sub");
        assert_eq!(r.registry, "myregistry:5000");
        assert_eq!(r.repository, "repo/sub");
        assert_eq!(r.reference, "latest");
    }

    #[test]
    fn parse_bearer_challenge() {
        let c =
            parse_www_authenticate(r#"Bearer realm="https://auth.docker.io/token",service="x""#)
                .unwrap();
        match c {
            AuthChallenge::Bearer { realm, service } => {
                assert_eq!(realm, "https://auth.docker.io/token");
                assert_eq!(service, "x");
            }
            AuthChallenge::Basic => panic!("expected Bearer"),
        }
    }

    #[test]
    fn parse_basic_challenge_ecr_shape() {
        // ECR sends `Basic realm="...",service="ecr.amazonaws.com"`.
        let c = parse_www_authenticate(
            r#"Basic realm="https://1234.dkr.ecr.eu-central-1.amazonaws.com/",service="ecr.amazonaws.com""#,
        )
        .unwrap();
        assert!(matches!(c, AuthChallenge::Basic));
    }

    #[test]
    fn parse_unknown_scheme_rejected() {
        let err = parse_www_authenticate("Digest realm=x").unwrap_err();
        assert!(
            err.to_string().contains("Digest"),
            "error should mention the scheme: {err}"
        );
    }
}
