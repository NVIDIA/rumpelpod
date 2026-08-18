// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Docker Compose model rendering and project lifecycle.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{ContainerEngine, Host};
use crate::devcontainer::{DevContainer, MountObject, MountType, StringOrArray};
use crate::image::{apply_docker_host, OutputLine};

const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";
const BUILD_CACHE_DOMAIN: &[u8] = b"rumpelpod compose build cache v1";

pub struct Source {
    files: Vec<PathBuf>,
    working_dir: PathBuf,
}

impl Source {
    pub fn new(files: &StringOrArray, devcontainer_dir: &Path) -> Result<Self> {
        let entries = match files {
            StringOrArray::String(file) => vec![file.clone()],
            StringOrArray::Array(files) => files.clone(),
        };
        if entries.is_empty() {
            return Err(anyhow::anyhow!(
                "dockerComposeFile must name at least one compose file"
            ));
        }
        let mut resolved = Vec::with_capacity(entries.len());
        for entry in entries {
            let path = devcontainer_dir.join(&entry);
            if !path.is_file() {
                let path = path.display();
                return Err(anyhow::anyhow!("compose file '{path}' does not exist"));
            }
            resolved.push(path);
        }
        Ok(Self {
            files: resolved,
            working_dir: devcontainer_dir.to_path_buf(),
        })
    }

    pub fn render(
        &self,
        project_name: &str,
        host: &Host,
        docker_socket: Option<&Path>,
        ssh_auth_sock: Option<&Path>,
        client_env: &HashMap<String, String>,
    ) -> Result<Model> {
        let mut command = docker_compose_command(host, docker_socket)?;
        self.apply(&mut command, project_name);
        // Normalized output expands implicit network and volume names with
        // this project's name. Persisting those names would make a fork
        // reuse its source pod's resources instead of getting its own.
        command.args(["config", "--no-normalize", "--format", "json"]);
        command.current_dir(&self.working_dir);
        command.envs(client_env);
        if let Some(socket) = ssh_auth_sock {
            command.env("SSH_AUTH_SOCK", socket);
        }
        let output = command
            .output()
            .context("rendering docker compose configuration")?;
        let stdout = checked_output(output, "docker compose config")?;
        Model::parse(String::from_utf8(stdout).context("compose config returned non-UTF-8 JSON")?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &self,
        project_name: &str,
        host: &Host,
        docker_socket: Option<&Path>,
        ssh_auth_sock: Option<&Path>,
        services: &[String],
        client_env: &HashMap<String, String>,
        progress: &std::sync::mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let mut command = docker_compose_command(host, docker_socket)?;
        self.apply(&mut command, project_name);
        command.args(["build", "--with-dependencies"]);
        command.args(services);
        command.current_dir(&self.working_dir);
        command.envs(client_env);
        if let Some(socket) = ssh_auth_sock {
            command.env("SSH_AUTH_SOCK", socket);
        }
        run_with_progress(&mut command, "docker compose build", progress)
    }

    pub fn service_image_id(
        &self,
        project_name: &str,
        host: &Host,
        docker_socket: Option<&Path>,
        service: &str,
        image_hint: Option<&str>,
    ) -> Result<String> {
        let default_image = format!("{project_name}-{service}");
        let image = image_hint.unwrap_or(&default_image);
        let mut command = docker_command(host, docker_socket)?;
        let output = command
            .args(["image", "inspect", "--format", "{{.Id}}", image])
            .output()
            .context("querying compose service image")?;
        let stdout = checked_output(output, "docker image inspect")?;
        let image_id = String::from_utf8_lossy(&stdout).trim().to_string();
        if image_id.is_empty() {
            return Err(anyhow::anyhow!(
                "docker image inspect returned no ID for compose service '{service}'"
            ));
        }
        Ok(image_id)
    }

    pub fn tag_service_image(
        &self,
        host: &Host,
        docker_socket: Option<&Path>,
        service: &str,
        image: &str,
        tag: &str,
    ) -> Result<()> {
        let mut command = docker_command(host, docker_socket)?;
        let output = command
            .args(["image", "tag", image, tag])
            .output()
            .with_context(|| format!("tagging compose service '{service}' build cache image"))?;
        checked_output(output, "docker image tag")?;
        Ok(())
    }

    fn apply(&self, command: &mut Command, project_name: &str) {
        command.args(["--project-name", project_name]);
        for file in &self.files {
            command.arg("--file").arg(file);
        }
    }
}

#[derive(Clone)]
pub struct Model {
    json: String,
    value: Value,
}

impl Model {
    pub fn parse(json: String) -> Result<Self> {
        let value: Value = serde_json::from_str(&json).context("parsing rendered compose JSON")?;
        let services = value
            .get("services")
            .and_then(Value::as_object)
            .context("rendered compose config has no services object")?;
        if services.is_empty() {
            return Err(anyhow::anyhow!(
                "rendered compose config contains no services"
            ));
        }
        Ok(Self { json, value })
    }

    pub fn json(&self) -> &str {
        &self.json
    }

    pub fn services(&self) -> HashSet<String> {
        self.value
            .get("services")
            .and_then(Value::as_object)
            .expect("services checked while parsing compose model")
            .keys()
            .cloned()
            .collect()
    }

    pub fn validate_service(&self, service: &str) -> Result<()> {
        if self.services().contains(service) {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "compose service '{service}' does not exist in the rendered configuration"
        ))
    }

    pub fn service_has_build(&self, service: &str) -> Result<bool> {
        Ok(self
            .service(service)?
            .get("build")
            .is_some_and(|build| !build.is_null()))
    }

    /// Compute project-independent tags for the images produced by Compose.
    ///
    /// A single project fingerprint deliberately covers every build service:
    /// Compose can make one service's output depend on another through build
    /// contexts, so invalidating the project as a unit avoids reusing a stale
    /// dependent image. Remote contexts and secret or SSH inputs remain mutable
    /// without a local content digest and therefore retain Compose's normal
    /// build behavior.
    pub fn build_cache_tags(&self) -> Result<Option<BTreeMap<String, String>>> {
        let services = self
            .value
            .get("services")
            .and_then(Value::as_object)
            .expect("services checked while parsing compose model");
        let mut build_services: Vec<&str> = services
            .iter()
            .filter_map(|(name, service)| {
                service
                    .get("build")
                    .is_some_and(|build| !build.is_null())
                    .then_some(name.as_str())
            })
            .collect();
        build_services.sort_unstable();

        let mut hasher = Sha256::new();
        hasher.update(BUILD_CACHE_DOMAIN);
        let mut contexts = BTreeSet::new();
        let mut dockerfiles = BTreeSet::new();
        for service_name in &build_services {
            let service = &services[*service_name];
            let build = service.get("build").expect("build service selected above");
            let build_object = build.as_object().with_context(|| {
                format!("compose service '{service_name}' has a non-object build configuration")
            })?;
            hash_json_value(&mut hasher, &Value::String((*service_name).to_string()));
            hash_json_value(&mut hasher, build);
            if let Some(platform) = service.get("platform") {
                hasher.update(b"service platform");
                hash_json_value(&mut hasher, platform);
            }
            if build_object
                .get("secrets")
                .is_some_and(|value| !value.is_null())
                || build_object
                    .get("ssh")
                    .is_some_and(|value| !value.is_null())
                || build_object.get("no_cache").and_then(Value::as_bool) == Some(true)
                || build_object.get("pull").and_then(Value::as_bool) == Some(true)
            {
                return Ok(None);
            }
            if let Some(args) = build_object.get("args") {
                let has_unresolved_arg = match args {
                    Value::Object(args) => args.values().any(Value::is_null),
                    Value::Array(args) => {
                        let mut unresolved = false;
                        for arg in args {
                            let arg = arg.as_str().with_context(|| {
                                format!(
                                    "compose service '{service_name}' has an invalid build argument"
                                )
                            })?;
                            unresolved |= !arg.contains('=');
                        }
                        unresolved
                    }
                    Value::Null => false,
                    Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                        return Err(anyhow::anyhow!(
                            "compose service '{service_name}' has invalid build arguments"
                        ));
                    }
                };
                if has_unresolved_arg {
                    return Ok(None);
                }
            }

            let context = build_object
                .get("context")
                .and_then(Value::as_str)
                .with_context(|| {
                    format!("compose service '{service_name}' build has no context path")
                })?;
            let Some(context_path) = local_build_context(context, false)? else {
                return Ok(None);
            };
            contexts.insert(context_path.clone());

            if build_object.get("dockerfile_inline").is_none() {
                let dockerfile = build_object
                    .get("dockerfile")
                    .map(|value| {
                        value.as_str().with_context(|| {
                            format!("compose service '{service_name}' has a non-string dockerfile")
                        })
                    })
                    .transpose()?
                    .unwrap_or("Dockerfile");
                dockerfiles.insert(context_path.join(dockerfile));
            }

            if let Some(additional_contexts) = build_object.get("additional_contexts") {
                let values: Vec<&str> = match additional_contexts {
                    Value::Object(entries) => entries
                        .values()
                        .map(|value| {
                            value.as_str().with_context(|| {
                                format!(
                                    "compose service '{service_name}' has a non-string additional build context"
                                )
                            })
                        })
                        .collect::<Result<_>>()?,
                    Value::Array(entries) => entries
                        .iter()
                        .map(|value| {
                            let entry = value.as_str().with_context(|| {
                                format!(
                                    "compose service '{service_name}' has a non-string additional build context"
                                )
                            })?;
                            entry.split_once('=').map(|(_, path)| path).with_context(|| {
                                format!(
                                    "compose service '{service_name}' has an invalid additional build context '{entry}'"
                                )
                            })
                        })
                        .collect::<Result<_>>()?,
                    Value::Null => Vec::new(),
                    Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                        return Err(anyhow::anyhow!(
                            "compose service '{service_name}' has invalid additional build contexts"
                        ));
                    }
                };
                for value in values {
                    if let Some(path) = local_build_context(value, true)? {
                        contexts.insert(path);
                    } else if !value.starts_with("service:") {
                        return Ok(None);
                    }
                }
            }
        }

        for context in contexts {
            hasher.update(b"context");
            hasher.update(context.as_os_str().as_encoded_bytes());
            crate::image::hash_context_dir(&mut hasher, &context).with_context(|| {
                let context = context.display();
                format!("hashing compose build context at {context}")
            })?;
        }
        for dockerfile in dockerfiles {
            hasher.update(b"dockerfile");
            hasher.update(dockerfile.as_os_str().as_encoded_bytes());
            let contents = fs::read(&dockerfile).with_context(|| {
                let dockerfile = dockerfile.display();
                format!("reading compose Dockerfile at {dockerfile}")
            })?;
            hasher.update((contents.len() as u64).to_le_bytes());
            hasher.update(contents);
        }

        let project_digest = hasher.finalize();
        let mut tags = BTreeMap::new();
        for service_name in build_services {
            let mut service_hasher = Sha256::new();
            service_hasher.update(BUILD_CACHE_DOMAIN);
            service_hasher.update(project_digest);
            service_hasher.update(service_name.as_bytes());
            let digest = service_hasher.finalize();
            tags.insert(
                service_name.to_string(),
                format!("rumpelpod-compose-{}", hex::encode(&digest[..8])),
            );
        }
        Ok(Some(tags))
    }

    pub fn service_image(&self, service: &str) -> Result<&str> {
        self.service(service)?
            .get("image")
            .and_then(Value::as_str)
            .with_context(|| {
                format!("compose agent service '{service}' has neither image nor build")
            })
    }

    pub fn service_image_optional(&self, service: &str) -> Result<Option<&str>> {
        Ok(self.service(service)?.get("image").and_then(Value::as_str))
    }

    pub fn service_user_optional(&self, service: &str) -> Result<Option<&str>> {
        Ok(self.service(service)?.get("user").and_then(Value::as_str))
    }

    pub fn published_port_services(&self) -> Vec<String> {
        self.value
            .get("services")
            .and_then(Value::as_object)
            .expect("services checked while parsing compose model")
            .iter()
            .filter(|(_, service)| {
                service
                    .get("ports")
                    .and_then(Value::as_array)
                    .is_some_and(|ports| !ports.is_empty())
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn validate_remote_bind_mounts(&self) -> Result<()> {
        let services = self
            .value
            .get("services")
            .and_then(Value::as_object)
            .expect("services checked while parsing compose model");
        for (service_name, service) in services {
            let Some(volumes) = service.get("volumes") else {
                continue;
            };
            let volumes = volumes
                .as_array()
                .with_context(|| format!("compose service '{service_name}' has invalid volumes"))?;
            for volume in volumes {
                let volume = volume.as_object().with_context(|| {
                    format!("compose service '{service_name}' has an invalid volume entry")
                })?;
                if volume.get("type").and_then(Value::as_str) != Some("bind") {
                    continue;
                }
                let source = volume
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                return Err(anyhow::anyhow!(
                    "compose service '{service_name}' uses bind mount '{source}', which is not supported with a remote Docker host; use a named volume or bake the files into the image"
                ));
            }
        }
        Ok(())
    }

    pub fn services_started_with(&self, selected: &[String]) -> Result<HashSet<String>> {
        let services = self
            .value
            .get("services")
            .and_then(Value::as_object)
            .expect("services checked while parsing compose model");
        let mut started = HashSet::new();
        let mut pending = selected.to_vec();
        while let Some(service_name) = pending.pop() {
            self.validate_service(&service_name)?;
            if !started.insert(service_name.clone()) {
                continue;
            }
            let service = services[&service_name]
                .as_object()
                .with_context(|| format!("compose service '{service_name}' is not an object"))?;
            if let Some(depends_on) = service.get("depends_on") {
                match depends_on {
                    Value::Object(dependencies) => pending.extend(dependencies.keys().cloned()),
                    Value::Array(dependencies) => {
                        for dependency in dependencies {
                            pending.push(
                                dependency
                                    .as_str()
                                    .with_context(|| {
                                        format!(
                                            "compose service '{service_name}' has an invalid depends_on entry"
                                        )
                                    })?
                                    .to_string(),
                            );
                        }
                    }
                    Value::Null => {}
                    Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                        return Err(anyhow::anyhow!(
                            "compose service '{service_name}' has invalid depends_on"
                        ));
                    }
                }
            }
            for field in ["links", "volumes_from"] {
                let Some(references) = service.get(field) else {
                    continue;
                };
                match references {
                    Value::Array(references) => {
                        for reference in references {
                            let reference = reference.as_str().with_context(|| {
                                format!(
                                    "compose service '{service_name}' has an invalid {field} entry"
                                )
                            })?;
                            if field == "volumes_from" && reference.starts_with("container:") {
                                continue;
                            }
                            let dependency = reference.split(':').next().unwrap_or(reference);
                            pending.push(dependency.to_string());
                        }
                    }
                    Value::Null => {}
                    Value::Bool(_) | Value::Number(_) | Value::Object(_) | Value::String(_) => {
                        return Err(anyhow::anyhow!(
                            "compose service '{service_name}' has invalid {field}"
                        ));
                    }
                }
            }
            for field in ["ipc", "network_mode", "pid"] {
                let Some(reference) = service.get(field) else {
                    continue;
                };
                match reference {
                    Value::String(reference) => {
                        if let Some(dependency) = reference.strip_prefix("service:") {
                            pending.push(dependency.to_string());
                        }
                    }
                    Value::Null => {}
                    Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
                        return Err(anyhow::anyhow!(
                            "compose service '{service_name}' has invalid {field}"
                        ));
                    }
                }
            }
        }
        Ok(started)
    }

    pub fn set_built_service_image(&mut self, service: &str, image: String) -> Result<()> {
        let service = self
            .value
            .get_mut("services")
            .and_then(Value::as_object_mut)
            .and_then(|services| services.get_mut(service))
            .and_then(Value::as_object_mut)
            .with_context(|| format!("compose service '{service}' is not an object"))?;
        service.insert("image".to_string(), Value::String(image));
        // The persisted runtime model must not need the original build
        // context. This also prevents a fork from rebuilding or pulling a
        // source service under its new project identity.
        service.remove("build");
        service.insert(
            "pull_policy".to_string(),
            Value::String("never".to_string()),
        );
        self.json =
            serde_json::to_string(&self.value).context("serializing rendered compose model")?;
        Ok(())
    }

    fn service(&self, service: &str) -> Result<&Value> {
        self.value
            .get("services")
            .and_then(Value::as_object)
            .and_then(|services| services.get(service))
            .with_context(|| format!("compose service '{service}' does not exist"))
    }
}

fn local_build_context(value: &str, allow_service: bool) -> Result<Option<PathBuf>> {
    if allow_service && value.starts_with("service:") {
        return Ok(None);
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Ok(None);
    }
    if !path.is_dir() {
        let path = path.display();
        return Err(anyhow::anyhow!(
            "compose build context '{path}' is not a directory"
        ));
    }
    Ok(Some(path))
}

fn hash_json_value(hasher: &mut Sha256, value: &Value) {
    match value {
        Value::Null => hasher.update(b"null"),
        Value::Bool(value) => {
            hasher.update(b"bool");
            hasher.update([u8::from(*value)]);
        }
        Value::Number(value) => {
            let value = value.to_string();
            hasher.update(b"number");
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        Value::String(value) => {
            hasher.update(b"string");
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        Value::Array(values) => {
            hasher.update(b"array");
            hasher.update((values.len() as u64).to_le_bytes());
            for value in values {
                hash_json_value(hasher, value);
            }
        }
        Value::Object(values) => {
            hasher.update(b"object");
            hasher.update((values.len() as u64).to_le_bytes());
            let mut keys: Vec<&String> = values.keys().collect();
            keys.sort_unstable();
            for key in keys {
                hash_json_value(hasher, &Value::String(key.clone()));
                hash_json_value(hasher, &values[key]);
            }
        }
    }
}

pub struct Project {
    name: String,
    model: Model,
    override_yaml: String,
    host: Host,
    docker_socket: Option<PathBuf>,
    client_env: HashMap<String, String>,
}

impl Project {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &str,
        model: Model,
        agent_service: &str,
        pod_name: &str,
        prepared_image: &str,
        repo_path: &Path,
        container_repo_path: &Path,
        devcontainer: &DevContainer,
        mounts: &[MountObject],
        host: &Host,
        docker_socket: Option<&Path>,
        client_env: &HashMap<String, String>,
    ) -> Result<Self> {
        model.validate_service(agent_service)?;
        let override_yaml = generate_override(
            &model,
            agent_service,
            pod_name,
            prepared_image,
            repo_path,
            container_repo_path,
            devcontainer,
            mounts,
        )?;
        Ok(Self {
            name: name.to_string(),
            model,
            override_yaml,
            host: host.clone(),
            docker_socket: docker_socket.map(Path::to_path_buf),
            client_env: client_env.clone(),
        })
    }

    pub fn up(
        &self,
        services: &[String],
        progress: &std::sync::mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        self.run_progress(
            "docker compose up",
            &["up", "--detach", "--no-build"],
            services,
            progress,
        )
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn start(&self) -> Result<()> {
        self.run("docker compose start", &["start"], &[])?;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.run("docker compose stop", &["stop", "--timeout", "0"], &[])?;
        Ok(())
    }

    pub fn down(&self) -> Result<()> {
        self.run(
            "docker compose down",
            &["down", "--volumes", "--remove-orphans"],
            &[],
        )?;
        Ok(())
    }

    pub fn service_containers(&self, service: &str) -> Result<Vec<String>> {
        self.model.validate_service(service)?;
        let stdout = self.run(
            "docker compose ps",
            &["ps", "--all", "--quiet"],
            &[service.to_string()],
        )?;
        Ok(String::from_utf8_lossy(&stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect())
    }

    pub fn one_service_container(&self, service: &str) -> Result<String> {
        let containers = self.service_containers(service)?;
        match containers.as_slice() {
            [container] => Ok(container.clone()),
            [] => Err(anyhow::anyhow!(
                "compose service '{service}' has no container in project '{}'",
                self.name
            )),
            _ => Err(anyhow::anyhow!(
                "compose service '{service}' has {} containers in project '{}'; expected exactly one",
                containers.len(),
                self.name
            )),
        }
    }

    pub fn inject_rumpel_into_sidecars(&self, agent_service: &str) -> Result<()> {
        let agent = self.one_service_container(agent_service)?;
        let stdout = self.run("docker compose ps", &["ps", "--all", "--quiet"], &[])?;
        for container in String::from_utf8_lossy(&stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if container == agent {
                continue;
            }
            let service = self.container_service(container)?;
            self.inject_one(container, &service)?;
        }
        Ok(())
    }

    fn inject_one(&self, container: &str, service: &str) -> Result<()> {
        let architecture = self.container_architecture(container)?;
        let binary = crate::prepared_image::find_rumpel_binary(&architecture)
            .with_context(|| format!("selecting rumpel binary for compose service '{service}'"))?;
        let staged = tempfile::tempdir().context("creating sidecar binary staging directory")?;
        let destination = staged.path().join("opt/rumpelpod/bin/rumpel");
        let parent = destination
            .parent()
            .expect("staged rumpel destination has a parent");
        fs::create_dir_all(parent).context("creating sidecar binary staging path")?;
        fs::copy(&binary, &destination).with_context(|| {
            format!(
                "staging rumpel binary for compose service '{service}' from {}",
                binary.display()
            )
        })?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))
            .context("making staged sidecar rumpel binary executable")?;

        let mut command = docker_command(&self.host, self.docker_socket.as_deref())?;
        let source = format!("{}/.", staged.path().display());
        let destination_arg = format!("{container}:/");
        let output = command
            .args(["cp", &source, &destination_arg])
            .output()
            .context("copying rumpel binary into compose sidecar")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(anyhow::anyhow!(
                "could not inject rumpel into compose service '{service}': {stderr}. If the service uses read_only, add a writable mount at /opt/rumpelpod"
            ));
        }
        Ok(())
    }

    fn container_service(&self, container: &str) -> Result<String> {
        let mut command = docker_command(&self.host, self.docker_socket.as_deref())?;
        let format_arg = format!("{{{{ index .Config.Labels {COMPOSE_SERVICE_LABEL:?} }}}}");
        let output = command
            .args(["container", "inspect", "--format", &format_arg, container])
            .output()
            .context("inspecting compose service label")?;
        let stdout = checked_output(output, "docker container inspect")?;
        let service = String::from_utf8_lossy(&stdout).trim().to_string();
        if service.is_empty() {
            return Err(anyhow::anyhow!(
                "compose container '{container}' has no {COMPOSE_SERVICE_LABEL} label"
            ));
        }
        Ok(service)
    }

    fn container_architecture(&self, container: &str) -> Result<String> {
        let mut command = docker_command(&self.host, self.docker_socket.as_deref())?;
        let output = command
            .args(["container", "inspect", container])
            .output()
            .context("inspecting compose container architecture")?;
        let stdout = checked_output(output, "docker container inspect")?;
        let mut inspected: Vec<ContainerInspect> =
            serde_json::from_slice(&stdout).context("parsing docker container inspect")?;
        let inspected = inspected
            .pop()
            .context("docker container inspect returned no containers")?;
        if let Some(architecture) = inspected
            .image_manifest_descriptor
            .and_then(|descriptor| descriptor.platform)
            .and_then(|platform| platform.architecture)
        {
            return validate_architecture(architecture);
        }

        let image = inspected
            .image
            .context("docker container inspect response has no Image")?;
        let mut image_command = docker_command(&self.host, self.docker_socket.as_deref())?;
        let output = image_command
            .args(["image", "inspect", &image])
            .output()
            .context("inspecting compose container image architecture")?;
        let stdout = checked_output(output, "docker image inspect")?;
        let mut images: Vec<ImageInspect> =
            serde_json::from_slice(&stdout).context("parsing docker image inspect")?;
        let architecture = images
            .pop()
            .and_then(|image| image.architecture)
            .context("docker image inspect response has no Architecture")?;
        validate_architecture(architecture)
    }

    fn run(&self, label: &str, args: &[&str], services: &[String]) -> Result<Vec<u8>> {
        let materialized = self.materialize()?;
        let mut command = docker_compose_command(&self.host, self.docker_socket.as_deref())?;
        apply_project_files(&mut command, &self.name, &materialized);
        command.args(args);
        command.args(services);
        command.envs(&self.client_env);
        let output = command
            .output()
            .with_context(|| format!("running {label}"))?;
        checked_output(output, label)
    }

    fn run_progress(
        &self,
        label: &str,
        args: &[&str],
        services: &[String],
        progress: &std::sync::mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let materialized = self.materialize()?;
        let mut command = docker_compose_command(&self.host, self.docker_socket.as_deref())?;
        apply_project_files(&mut command, &self.name, &materialized);
        command.args(args);
        command.args(services);
        command.envs(&self.client_env);
        run_with_progress(&mut command, label, progress)
    }

    fn materialize(&self) -> Result<MaterializedProject> {
        let dir = tempfile::tempdir().context("creating compose project materialization")?;
        let model_path = dir.path().join("compose.json");
        let override_path = dir.path().join("rumpelpod.compose.yaml");
        fs::write(&model_path, self.model.json()).context("writing rendered compose model")?;
        fs::write(&override_path, &self.override_yaml).context("writing compose override")?;
        Ok(MaterializedProject {
            _dir: dir,
            model_path,
            override_path,
        })
    }
}

struct MaterializedProject {
    _dir: tempfile::TempDir,
    model_path: PathBuf,
    override_path: PathBuf,
}

#[derive(Deserialize)]
struct ContainerInspect {
    #[serde(rename = "Image")]
    image: Option<String>,
    #[serde(rename = "ImageManifestDescriptor")]
    image_manifest_descriptor: Option<ImageManifestDescriptor>,
}

#[derive(Deserialize)]
struct ImageManifestDescriptor {
    #[serde(rename = "Platform")]
    platform: Option<ImagePlatform>,
}

#[derive(Deserialize)]
struct ImagePlatform {
    #[serde(rename = "Architecture")]
    architecture: Option<String>,
}

#[derive(Deserialize)]
struct ImageInspect {
    #[serde(rename = "Architecture")]
    architecture: Option<String>,
}

fn validate_architecture(architecture: String) -> Result<String> {
    match architecture.as_str() {
        "amd64" | "arm64" => Ok(architecture),
        _ => Err(anyhow::anyhow!(
            "unsupported compose container architecture '{architecture}'; expected amd64 or arm64"
        )),
    }
}

fn docker_command(host: &Host, docker_socket: Option<&Path>) -> Result<Command> {
    match host.container_engine() {
        Some(ContainerEngine::Docker) => {}
        Some(ContainerEngine::Podman) => {
            return Err(anyhow::anyhow!(
                "dockerComposeFile is supported only with the Docker engine, not Podman"
            ));
        }
        Some(ContainerEngine::Auto) => {
            panic!("container engine auto remained after resolve")
        }
        None => {
            return Err(anyhow::anyhow!(
                "dockerComposeFile is supported only with the Docker engine, not Kubernetes"
            ));
        }
    }
    let mut command = Command::new("docker");
    apply_docker_host(&mut command, host, docker_socket);
    Ok(command)
}

fn docker_compose_command(host: &Host, docker_socket: Option<&Path>) -> Result<Command> {
    let mut command = docker_command(host, docker_socket)?;
    command.arg("compose");
    Ok(command)
}

fn apply_project_files(command: &mut Command, project_name: &str, files: &MaterializedProject) {
    command.args(["--project-name", project_name]);
    command.arg("--file").arg(&files.model_path);
    command.arg("--file").arg(&files.override_path);
}

fn checked_output(output: Output, label: &str) -> Result<Vec<u8>> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(anyhow::anyhow!("{label} failed: {stderr}"))
}

fn run_with_progress(
    command: &mut Command,
    label: &str,
    progress: &std::sync::mpsc::Sender<OutputLine>,
) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("running {label}"))?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        progress.send(OutputLine::Stdout(line.to_string())).ok();
    }
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        progress.send(OutputLine::Stderr(line.to_string())).ok();
    }
    checked_output(output, label)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn generate_override(
    model: &Model,
    agent_service: &str,
    pod_name: &str,
    prepared_image: &str,
    repo_path: &Path,
    container_repo_path: &Path,
    devcontainer: &DevContainer,
    mounts: &[MountObject],
) -> Result<String> {
    let mut yaml = String::from("services:\n");
    let repo_path = repo_path.display().to_string();
    let container_repo_path = container_repo_path.display().to_string();
    let mut volume_keys: HashSet<String> = model
        .value
        .get("volumes")
        .and_then(Value::as_object)
        .map(|volumes| volumes.keys().cloned().collect())
        .unwrap_or_default();
    let mut named_mounts = BTreeMap::new();
    for source in mounts
        .iter()
        .filter(|mount| mount.mount_type == MountType::Volume)
        .filter_map(|mount| mount.source.as_ref())
    {
        if named_mounts.contains_key(source) {
            continue;
        }
        let mut index = named_mounts.len();
        let logical_name = loop {
            let candidate = format!("rumpelpod_mount_{index}");
            if volume_keys.insert(candidate.clone()) {
                break candidate;
            }
            index += 1;
        };
        named_mounts.insert(source.clone(), logical_name);
    }
    let mut services: Vec<String> = model.services().into_iter().collect();
    services.sort();
    for service in services {
        yaml.push_str(&format!("  {}:\n", yaml_scalar(&service)));
        yaml.push_str("    ports: !reset []\n");
        yaml.push_str("    container_name: !reset null\n");
        if service == agent_service {
            // The prepared agent image is local. A source pull policy must
            // not turn its reference into a registry pull during reconnect.
            yaml.push_str("    pull_policy: never\n");
        }
        yaml.push_str("    labels:\n");
        yaml.push_str(&format!(
            "      {}: {}\n",
            yaml_scalar(crate::executor::LABEL_DOCKER_REPO_PATH),
            yaml_scalar(&repo_path)
        ));
        if service != agent_service {
            continue;
        }

        yaml.push_str(&format!(
            "      {}: {}\n",
            yaml_scalar(crate::executor::LABEL_DOCKER_CONTAINER_REPO_PATH),
            yaml_scalar(&container_repo_path)
        ));
        yaml.push_str(&format!(
            "      {}: {}\n",
            yaml_scalar(crate::executor::LABEL_DOCKER_POD_NAME),
            yaml_scalar(pod_name)
        ));
        yaml.push_str(&format!("    image: {}\n", yaml_scalar(prepared_image)));
        yaml.push_str("    environment:\n");
        let mut environment = BTreeMap::new();
        if let Some(container_environment) = &devcontainer.container_env {
            for (key, value) in container_environment {
                environment.insert(key.as_str(), value.as_str());
            }
        }
        environment.insert("SSH_AUTH_SOCK", crate::pod::SSH_AGENT_SOCK_PATH);
        for (key, value) in environment {
            yaml.push_str(&format!(
                "      {}: {}\n",
                yaml_scalar(key),
                yaml_scalar(value)
            ));
        }
        if let Some(user) = devcontainer.user() {
            yaml.push_str(&format!("    user: {}\n", yaml_scalar(user)));
        }
        if !mounts.is_empty() {
            yaml.push_str("    volumes:\n");
            for mount in mounts {
                let mount_type = match mount.mount_type {
                    MountType::Bind => "bind",
                    MountType::Volume => "volume",
                    MountType::Tmpfs => "tmpfs",
                };
                yaml.push_str(&format!("      - type: {mount_type}\n"));
                let source = match mount.mount_type {
                    MountType::Volume => mount
                        .source
                        .as_ref()
                        .and_then(|source| named_mounts.get(source))
                        .map(String::as_str),
                    MountType::Bind | MountType::Tmpfs => mount.source.as_deref(),
                };
                if let Some(source) = source {
                    yaml.push_str(&format!("        source: {}\n", yaml_scalar(source)));
                }
                yaml.push_str(&format!("        target: {}\n", yaml_scalar(&mount.target)));
                if mount.read_only == Some(true) {
                    yaml.push_str("        read_only: true\n");
                }
            }
        }
        if devcontainer.privileged == Some(true) {
            yaml.push_str("    privileged: true\n");
        }
        if devcontainer.init == Some(true) {
            yaml.push_str("    init: true\n");
        }
        if let Some(cap_add) = &devcontainer.cap_add {
            yaml.push_str(&format!(
                "    cap_add: {}\n",
                serde_json::to_string(cap_add).context("serializing compose capAdd")?
            ));
        }
        if let Some(security_opt) = &devcontainer.security_opt {
            yaml.push_str(&format!(
                "    security_opt: {}\n",
                serde_json::to_string(security_opt).context("serializing compose securityOpt")?
            ));
        }
        if devcontainer.override_command == Some(true) {
            yaml.push_str("    entrypoint: []\n");
            yaml.push_str("    command: [\"sleep\", \"infinity\"]\n");
        }
    }
    if !named_mounts.is_empty() {
        yaml.push_str("volumes:\n");
        for (source, logical_name) in named_mounts {
            yaml.push_str(&format!("  {}:\n", yaml_scalar(&logical_name)));
            yaml.push_str(&format!("    name: {}\n", yaml_scalar(&source)));
        }
    }
    Ok(yaml)
}

fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).expect("serializing string to a YAML-compatible scalar")
}

pub fn selected_services(devcontainer: &DevContainer, agent_service: &str) -> Vec<String> {
    let Some(run_services) = devcontainer.run_services.as_ref() else {
        return Vec::new();
    };
    let mut services = run_services.clone();
    if !services.iter().any(|service| service == agent_service) {
        services.push(agent_service.to_string());
    }
    services
}

pub fn validate_selected_services(
    model: &Model,
    agent_service: &str,
    selected: &[String],
) -> Result<()> {
    model.validate_service(agent_service)?;
    for service in selected {
        model.validate_service(service)?;
    }
    Ok(())
}
