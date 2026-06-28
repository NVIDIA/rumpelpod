// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Imperative install/delete/status for rumpelhub.
//!
//! `install` builds the hub image, renders a fixed
//! ServiceAccount+Deployment+Service from a [`HubInstallSpec`], and
//! server-side-applies them.  `delete` removes the same resources
//! by label selector.  `status` port-forwards to `svc/rumpelhub`
//! and fetches `/healthz`.

use std::time::Duration;

use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Service, ServiceAccount};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};

use super::image::build_hub_image;
use super::manifests::HubInstallSpec;
use super::{HUB_NAME, HUB_PORT, HUB_SELECTOR};
use crate::async_runtime::block_on;
use crate::config::{ContainerEngine, Host};

/// Resolve a `Host::Kubernetes` from CLI flags or the `.rumpelpod.json`
/// config in the current directory.
///
/// Unlike `rumpel enter`, hub commands do not need a devcontainer.
pub fn resolve_kubernetes_host(
    cli_context: Option<&str>,
    cli_namespace: Option<&str>,
    cli_registry: Option<&str>,
) -> Result<Host> {
    if let Some(context) = cli_context {
        let namespace = cli_namespace.unwrap_or("default").to_string();
        let registry = cli_registry
            .expect("clap `requires = kubernetes_registry` ensures this is set")
            .to_string();
        let host = Host::Kubernetes {
            context: context.to_string(),
            namespace,
            registry,
            node_selector: None,
            tolerations: None,
            builder: None,
            image_builder: ContainerEngine::Auto,
        };
        return host.resolve_container_tools();
    }

    let cwd = std::env::current_dir().context("getting current directory")?;
    let config = crate::config::load_json_config(&cwd)?;
    let kubernetes = config.kubernetes.ok_or_else(|| {
        anyhow::anyhow!(
            "no Kubernetes host configured.  Pass \
             --kubernetes-context/--kubernetes-namespace/--kubernetes-registry, \
             or set 'kubernetes' in .rumpelpod.json"
        )
    })?;
    let namespace = cli_namespace
        .map(|s| s.to_string())
        .or(kubernetes.namespace)
        .unwrap_or_else(|| "default".to_string());
    let host = Host::Kubernetes {
        context: kubernetes.context,
        namespace,
        registry: kubernetes.registry,
        node_selector: kubernetes.node_selector,
        tolerations: kubernetes.tolerations,
        builder: kubernetes.builder,
        image_builder: config.container_engine.unwrap_or(ContainerEngine::Auto),
    };
    host.resolve_container_tools()
}

/// Build a `kube::Client` for the given context.  Mirrors what
/// `K8sClient::new` does in `src/k8s.rs`.
fn kube_client(context: &str) -> Result<Client> {
    block_on(async {
        let kubeconfig = Kubeconfig::read().context("reading kubeconfig")?;
        let config = Config::from_custom_kubeconfig(
            kubeconfig,
            &KubeConfigOptions {
                context: Some(context.to_string()),
                ..Default::default()
            },
        )
        .await
        .context("building kube config from kubeconfig")?;
        Client::try_from(config).context("creating kube client")
    })
}

/// Extract context and namespace out of a Kubernetes host, failing if
/// the host is not Kubernetes.
fn host_parts(host: &Host) -> Result<(&str, &str)> {
    match host {
        Host::Kubernetes {
            context, namespace, ..
        } => Ok((context.as_str(), namespace.as_str())),
        Host::Localhost { .. } | Host::Ssh { .. } => {
            Err(anyhow::anyhow!("rumpel hub only supports Kubernetes hosts"))
        }
    }
}

/// Install the hub into the target namespace.
pub fn install(host: &Host) -> Result<()> {
    let (context, namespace) = host_parts(host)?;
    let image = build_hub_image(host)?;
    eprintln!("rumpelhub image: {image}");

    let spec = HubInstallSpec {
        namespace: namespace.to_string(),
        image,
    };
    let resources = spec.manifests()?;

    let client = kube_client(context)?;
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), namespace);
    let dep_api: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let svc_api: Api<Service> = Api::namespaced(client.clone(), namespace);

    // Server-side apply keeps install idempotent: rerunning overwrites
    // fields we own without fighting over unrelated annotations.
    let pp = PatchParams::apply("rumpel-hub-install").force();

    block_on(async {
        sa_api
            .patch(HUB_NAME, &pp, &Patch::Apply(&resources.service_account))
            .await
            .context("applying hub ServiceAccount")?;
        dep_api
            .patch(HUB_NAME, &pp, &Patch::Apply(&resources.deployment))
            .await
            .context("applying hub Deployment")?;
        svc_api
            .patch(HUB_NAME, &pp, &Patch::Apply(&resources.service))
            .await
            .context("applying hub Service")?;
        Ok::<_, anyhow::Error>(())
    })?;

    eprintln!("applied rumpelhub resources in namespace {namespace}");
    wait_deployment_ready(host, Duration::from_secs(180))?;
    eprintln!("rumpelhub Deployment is Ready");
    Ok(())
}

/// Block until the rumpelhub Deployment reports `status.availableReplicas >= 1`.
fn wait_deployment_ready(host: &Host, timeout: Duration) -> Result<()> {
    let (context, namespace) = host_parts(host)?;
    let client = kube_client(context)?;
    let dep_api: Api<Deployment> = Api::namespaced(client, namespace);
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let dep = block_on(dep_api.get(HUB_NAME))
            .with_context(|| format!("getting Deployment/{HUB_NAME}"))?;
        let available = dep
            .status
            .as_ref()
            .and_then(|s| s.available_replicas)
            .unwrap_or(0);
        if available >= 1 {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "Deployment/{HUB_NAME} did not become available within {:?}",
                timeout
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Delete all hub-owned resources in the target namespace.  Idempotent:
/// the call is built around `delete_collection` which returns an
/// empty result (not a 404) when the label selector matches nothing,
/// so a delete before any install is a silent no-op by construction.
pub fn delete(host: &Host) -> Result<()> {
    let (context, namespace) = host_parts(host)?;
    let client = kube_client(context)?;
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), namespace);
    let dep_api: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let svc_api: Api<Service> = Api::namespaced(client.clone(), namespace);

    let lp = ListParams::default().labels(HUB_SELECTOR);
    let dp = DeleteParams::default();

    block_on(async {
        dep_api
            .delete_collection(&dp, &lp)
            .await
            .context("deleting hub Deployments")?;
        svc_api
            .delete_collection(&dp, &lp)
            .await
            .context("deleting hub Services")?;
        sa_api
            .delete_collection(&dp, &lp)
            .await
            .context("deleting hub ServiceAccounts")?;
        Ok::<_, anyhow::Error>(())
    })?;

    eprintln!("deleted rumpelhub resources in namespace {namespace}");
    Ok(())
}

/// Port-forward to the rumpelhub Service and GET `/healthz`.
pub fn status(host: &Host) -> Result<()> {
    let (context, namespace) = host_parts(host)?;
    let client = crate::k8s::K8sClient::new(context, namespace)?;

    // kubectl port-forward accepts `svc/<name>` as the target,
    // matching the way the existing K8sClient::port_forward drives it.
    let target = format!("svc/{HUB_NAME}");
    let handle = client.port_forward(&target, HUB_PORT)?;
    let url = format!("http://127.0.0.1:{}/healthz", handle.local_port);
    let response = reqwest::blocking::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    let body = response.text().context("reading /healthz body")?;
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "hub /healthz returned HTTP {status}: {body}"
        ));
    }
    println!("{body}");
    Ok(())
}
