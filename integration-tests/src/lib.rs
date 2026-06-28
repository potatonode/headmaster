use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::ListParams;
use kube::core::DynamicObject;
use kube::discovery::{Discovery, Scope};

/// Builds a [`kube::Client`] from the `KUBE_CONTEXT` environment variable, or
/// falls back to the default context if the variable is unset.
pub async fn kube_client() -> Result<kube::Client, Box<dyn std::error::Error>> {
    let config = match std::env::var("KUBE_CONTEXT") {
        Ok(ctx) => {
            kube::Config::from_kubeconfig(&kube::config::KubeConfigOptions {
                context: Some(ctx),
                ..Default::default()
            })
            .await?
        }
        Err(_) => kube::Config::infer().await?,
    };
    Ok(kube::Client::try_from(config)?)
}

/// Errors in `status.conditions[].reason` (or pod container waiting reasons)
/// that mean the resource will never recover without intervention.
const FATAL_REASONS: &[&str] = &[
    "ImagePullBackOff",
    "ErrImagePull",
    "InvalidImageName",
    "BackoffLimitExceeded",
];

/// Waits for every resource in `namespace` that exposes a `Ready` or
/// `Available` status condition to report `True`. Fails fast on image-pull
/// errors or persistent crash loops so callers surface real failures instead
/// of timing out after 5 minutes.
pub async fn wait_for_namespace_ready(
    kube: &kube::Client,
    namespace: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Discover all API resource types once — they don't change while we wait.
    let discovery = Discovery::new(kube.clone()).run().await?;
    let namespaced: Vec<_> = discovery
        .groups()
        .flat_map(|g| g.recommended_resources())
        .filter(|(_, caps)| caps.scope == Scope::Namespaced && caps.supports_operation("list"))
        .collect();

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let deadline = Instant::now() + Duration::from_secs(300);

    loop {
        // ── Fail fast on container-level errors that won't self-heal ──────────
        for pod in pods.list(&ListParams::default()).await?.items {
            let pod_name = pod.metadata.name.as_deref().unwrap_or("?");
            for cs in pod
                .status
                .iter()
                .flat_map(|s| s.container_statuses.iter())
                .flatten()
            {
                if let Some(w) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
                    let reason = w.reason.as_deref().unwrap_or("");
                    if FATAL_REASONS.contains(&reason) {
                        return Err(format!("{pod_name}/{}: {reason}", cs.name).into());
                    }
                    if reason == "CrashLoopBackOff" && cs.restart_count > 5 {
                        return Err(format!(
                            "{pod_name}/{}: CrashLoopBackOff after {} restarts",
                            cs.name, cs.restart_count
                        )
                        .into());
                    }
                }
            }
        }

        // ── Check Ready / Available conditions on all resources ───────────────
        let mut not_ready: Vec<String> = Vec::new();
        let mut found_any = false;

        for (ar, _) in &namespaced {
            let api: Api<DynamicObject> = Api::namespaced_with(kube.clone(), namespace, ar);
            let list = match api.list(&ListParams::default()).await {
                Ok(l) => l,
                Err(_) => continue, // no permission or not applicable — skip
            };
            for obj in list.items {
                let name = obj.metadata.name.as_deref().unwrap_or("?");
                let Some(conditions) = obj.data["status"]["conditions"].as_array() else {
                    continue;
                };
                // Only consider resources that expose a Ready or Available condition.
                let applicable: Vec<_> = conditions
                    .iter()
                    .filter(|c| matches!(c["type"].as_str(), Some("Ready" | "Available")))
                    .collect();
                if applicable.is_empty() {
                    continue;
                }
                found_any = true;
                if applicable
                    .iter()
                    .any(|c| c["status"].as_str() == Some("True"))
                {
                    continue; // at least one applicable condition is True
                }
                // Fail fast if any condition names a terminal reason.
                for c in &applicable {
                    let reason = c["reason"].as_str().unwrap_or("");
                    if FATAL_REASONS.contains(&reason) {
                        return Err(format!("{}/{name} not ready: {reason}", ar.kind).into());
                    }
                }
                not_ready.push(format!("{}/{name}", ar.kind));
            }
        }

        // Only declare success once we have seen at least one resource with
        // conditions — avoids a false-positive before any pods are scheduled.
        if found_any && not_ready.is_empty() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for namespace {namespace}; \
                 still not ready: {not_ready:?}"
            )
            .into());
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
