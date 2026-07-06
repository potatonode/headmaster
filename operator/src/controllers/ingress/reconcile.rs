//! Main reconcile loop for `Ingress`. Provisions a Tailscale proxy for every
//! `Ingress` annotated `ingressClassName: headmaster`, and cleans up all proxy
//! resources (StatefulSet, Service, RBAC, Secrets, ConfigMap) on deletion.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use headscale_client::headscale::v1::{DeleteNodeRequest, SetTagsRequest};
use headscale_client::{AuthenticatedClient, Code};
use k8s_ext::SecretGetExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Secret, Service, ServiceAccount};
use k8s_openapi::api::networking::v1::{Ingress, IngressClass, IngressClassSpec};
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Client, Resource, ResourceExt};

use super::auth_key::{AuthKeyStatus, ensure_auth_key};
use super::error::Error;
use super::names::{ProxyNames, ingress_auto_tag};
use super::proxy::{
    apply_proxy_rbac, apply_proxy_statefulset, apply_serve_configmap, apply_wireguard_service,
    collect_ingress_routes, ensure_state_secret, patch_ingress_status,
};
use super::{CONTROLLER_NAME, INGRESS_CLASS_NAME};
use crate::FINALIZER;
use crate::context::Context;
use crate::controllers::applier::{ChildApplier, delete_ignoring_404};
use crate::controllers::recorder::RecorderExt;
use crate::labels;
use crate::types::{HeadscaleInstance, IngressAnnotations, ResourceStatus};

// ── public entrypoints ────────────────────────────────────────────────────────

/// Ensures the `headmaster` IngressClass exists. Called once on startup.
pub async fn ensure_ingress_class(client: &Client) -> Result<(), kube::Error> {
    let ssa = PatchParams::apply(crate::FIELD_MANAGER).force();
    let desired = IngressClass {
        metadata: ObjectMeta {
            name: Some(INGRESS_CLASS_NAME.to_string()),
            ..Default::default()
        },
        spec: Some(IngressClassSpec {
            controller: Some(CONTROLLER_NAME.to_string()),
            ..Default::default()
        }),
    };
    Api::<IngressClass>::all(client.clone())
        .patch(INGRESS_CLASS_NAME, &ssa, &Patch::Apply(&desired))
        .await?;
    Ok(())
}

pub fn stream(
    ingress_api: Api<Ingress>,
    ctx: Arc<Context>,
    shutdown: impl std::future::Future<Output = ()> + Send + Sync + 'static,
) -> impl std::future::Future<Output = ()> {
    let controller = kube::runtime::Controller::new(ingress_api, watcher::Config::default());
    let ingress_store = controller.store();
    controller
        .watches(
            Api::<Secret>::namespaced(ctx.client.clone(), &ctx.operator_namespace),
            watcher::Config::default().labels(&format!(
                "{}={}",
                labels::APP_MANAGED_BY,
                labels::MANAGED_BY_VALUE
            )),
            |secret| {
                let ingress_name = secret.labels().get(labels::INGRESS_NAME)?.clone();
                let ingress_ns = secret.labels().get(labels::INGRESS_NAMESPACE)?.clone();
                Some(ObjectRef::<Ingress>::new(&ingress_name).within(&ingress_ns))
            },
        )
        .watches(
            Api::<HeadscaleInstance>::namespaced(ctx.client.clone(), &ctx.operator_namespace),
            watcher::Config::default(),
            move |instance| {
                // ingress_store.state() may be empty during the initial list/watch cycle
                // (before InitDone). In that case this closure returns no ObjectRefs, but
                // kube-runtime's trigger_self mechanism queues every Ingress for an initial
                // reconcile once the store is ready, so no Ingress is permanently missed.
                // In steady state the store is always populated and this works correctly.
                let instance_name = instance.name_any();
                ingress_store
                    .state()
                    .into_iter()
                    .filter(move |ing| {
                        IngressAnnotations::headscale_ref(ing).as_deref() == Some(&instance_name)
                    })
                    .map(|ing| ObjectRef::from_obj(&*ing))
            },
        )
        .graceful_shutdown_on(shutdown)
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::warn!("Ingress reconcile error: {e:?}");
            }
        })
}

// ── reconcile ─────────────────────────────────────────────────────────────────

fn error_policy(obj: Arc<Ingress>, e: &Error, _ctx: Arc<Context>) -> Action {
    tracing::warn!(name = obj.name_any(), "Ingress reconcile failed: {e:?}");
    Action::requeue(Duration::from_secs(30))
}

async fn reconcile(ingress: Arc<Ingress>, ctx: Arc<Context>) -> Result<Action, Error> {
    let class = ingress
        .spec
        .as_ref()
        .and_then(|s| s.ingress_class_name.as_deref())
        .or_else(|| {
            ingress
                .annotations()
                .get("kubernetes.io/ingress.class")
                .map(String::as_str)
        });
    if class != Some(INGRESS_CLASS_NAME) {
        return Ok(Action::await_change());
    }

    let ns = ingress.namespace().ok_or(Error::MissingNamespace)?;

    // Namespace filter: skip Ingresses in namespaces not on the watch list.
    // Exception: if the Ingress already has our finalizer we previously managed
    // it and must run apply() once to deregister its proxy and remove the
    // finalizer — otherwise the Ingress stays stuck with leaked resources.
    // Once the finalizer is gone the Ingress falls through to the early return
    // on every subsequent reconcile and is never touched again.
    let has_our_finalizer = ingress.finalizers().contains(&FINALIZER.to_string());
    if !ctx.ingress_watch_namespaces.is_empty()
        && !ctx.ingress_watch_namespaces.iter().any(|n| n == &ns)
        && !has_our_finalizer
    {
        return Ok(Action::await_change());
    }

    let api: Api<Ingress> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&api, FINALIZER, ingress, |event| async {
        match event {
            Finalizer::Apply(ing) => apply(ing, &ctx).await,
            Finalizer::Cleanup(ing) => cleanup(ing, &ctx).await,
        }
    })
    .await
    .map_err(|e| match e {
        kube::runtime::finalizer::Error::ApplyFailed(e) => e,
        kube::runtime::finalizer::Error::CleanupFailed(e) => e,
        kube::runtime::finalizer::Error::AddFinalizer(e) => Error::Kube(e),
        kube::runtime::finalizer::Error::RemoveFinalizer(e) => Error::Kube(e),
        kube::runtime::finalizer::Error::UnnamedObject => Error::UnnamedObject,
        kube::runtime::finalizer::Error::InvalidFinalizer => {
            panic!("BUG: '{}' is not a valid finalizer string", FINALIZER)
        }
    })
}

// ── apply ─────────────────────────────────────────────────────────────────────

async fn apply(ingress: Arc<Ingress>, ctx: &Context) -> Result<Action, Error> {
    let ingress_ns = ingress.namespace().unwrap_or_default();
    let ingress_name = ingress.name_any();
    let op_ns = &ctx.operator_namespace;

    // If this namespace was removed from INGRESS_WATCH_NAMESPACES after we
    // already provisioned this Ingress (detected by our finalizer still being
    // present), deregister proxy resources and then remove our finalizer so
    // the operator completely relinquishes control and never touches this
    // Ingress again.
    if !ctx.ingress_watch_namespaces.is_empty()
        && !ctx
            .ingress_watch_namespaces
            .iter()
            .any(|n| n == &ingress_ns)
    {
        let names = ProxyNames::new(&ingress_ns, &ingress_name);
        if let Ok(annotations) = IngressAnnotations::parse(&ingress) {
            deregister_and_cleanup(ctx, op_ns, &names, &ingress, &annotations.headscale_ref)
                .await?;
        } else {
            // No valid annotations means the Ingress was never fully provisioned;
            // just clean up any k8s resources and fall through to finalizer removal.
            cleanup_proxy_resources(ctx, op_ns, &names).await;
        }
        let remaining: Vec<String> = ingress
            .finalizers()
            .iter()
            .filter(|f| f.as_str() != FINALIZER)
            .cloned()
            .collect();
        Api::<Ingress>::namespaced(ctx.client.clone(), &ingress_ns)
            .patch(
                &ingress_name,
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": remaining } })),
            )
            .await
            .map_err(Error::Kube)?;
        return Ok(Action::await_change());
    }

    let annotations = IngressAnnotations::parse(&ingress)?;

    if namespace_is_deleting(&ctx.client, &ingress_ns).await? {
        tracing::info!(
            name = ingress_name,
            namespace = ingress_ns,
            "Ingress: namespace is deleting; skipping"
        );
        return Ok(Action::await_change());
    }

    // HeadscaleInstance lives in the operator namespace.
    let instance_api: Api<HeadscaleInstance> = Api::namespaced(ctx.client.clone(), op_ns);
    let instance = match instance_api.get(&annotations.headscale_ref).await {
        Ok(inst) => inst,
        Err(kube::Error::Api(ref e)) if e.code == 404 => {
            let recorder = ctx.recorder();
            let _ = recorder
                .publish_warning(
                    &ingress.object_ref(&()),
                    "Pending",
                    &format!(
                        "HeadscaleInstance '{}' does not exist",
                        annotations.headscale_ref
                    ),
                )
                .await;
            return Ok(Action::requeue(Duration::from_secs(30)));
        }
        Err(e) => return Err(Error::Kube(e)),
    };
    // Proxy resource names are scoped to operator namespace using
    // {ingress_ns}-{ingress_name} as the base to avoid cross-namespace collisions.
    let names = ProxyNames::new(&ingress_ns, &ingress_name);

    if !instance.spec.watched_namespaces.is_empty()
        && !instance.spec.watched_namespaces.contains(&ingress_ns)
    {
        let _ = ctx
            .recorder()
            .publish_warning(
                &ingress.object_ref(&()),
                "NamespaceExcluded",
                &format!(
                    "namespace '{}' is not in HeadscaleInstance \
                     '{}' watchedNamespaces; this Ingress is now orphaned",
                    ingress_ns, annotations.headscale_ref,
                ),
            )
            .await;
        let _ = Api::<Ingress>::namespaced(ctx.client.clone(), &ingress_ns)
            .patch_status(
                &ingress_name,
                &PatchParams::apply(crate::FIELD_MANAGER).force(),
                &Patch::Apply(serde_json::json!({
                    "apiVersion": "networking.k8s.io/v1",
                    "kind": "Ingress",
                    "metadata": { "name": ingress_name, "namespace": ingress_ns },
                    "status": {}
                })),
            )
            .await;
        deregister_and_cleanup(ctx, op_ns, &names, &ingress, &annotations.headscale_ref).await?;
        let remaining: Vec<String> = ingress
            .finalizers()
            .iter()
            .filter(|f| f.as_str() != FINALIZER)
            .cloned()
            .collect();
        Api::<Ingress>::namespaced(ctx.client.clone(), &ingress_ns)
            .patch(
                &ingress_name,
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": remaining } })),
            )
            .await
            .map_err(Error::Kube)?;
        return Ok(Action::await_change());
    }

    if !instance.status.as_ref().is_some_and(|s| s.is_ready()) {
        let recorder = ctx.recorder();
        let _ = recorder
            .publish_warning(
                &ingress.object_ref(&()),
                "Pending",
                &format!(
                    "HeadscaleInstance '{}' is not yet ready",
                    annotations.headscale_ref
                ),
            )
            .await;
        return Ok(Action::requeue(Duration::from_secs(5)));
    }

    let dns_base_domain = instance.spec.dns_base_domain.clone();
    let internal_headscale_url = format!(
        "http://headscale-server-{}.{op_ns}.svc.cluster.local:8080",
        annotations.headscale_ref,
    );
    let tailnet_fqdn = format!("{}.{dns_base_domain}", annotations.hostname);

    let child = ChildApplier::for_proxy(
        ctx,
        op_ns,
        &names.proxy_base,
        &instance,
        &ingress_name,
        &ingress_ns,
    );

    for grant in &annotations.access {
        if grant.from.is_empty() {
            let recorder = ctx.recorder();
            let _ = recorder
                .publish_warning(
                    &ingress.object_ref(&()),
                    "InvalidConfig",
                    "access grant 'from' must not be empty",
                )
                .await;
            return Ok(Action::await_change());
        }
    }

    let auto_tag = if !annotations.access.is_empty() {
        Some(ingress_auto_tag(&ingress_ns, &ingress_name))
    } else {
        None
    };

    let cap_names: Vec<String> = {
        let mut unique_caps: std::collections::BTreeSet<String> = Default::default();
        for grant in &annotations.access {
            if let Some(caps) = &grant.capabilities {
                unique_caps.extend(caps.keys().cloned());
            }
        }
        unique_caps.into_iter().collect()
    };

    let routes = match collect_ingress_routes(&ctx.client, &ingress, &ingress_ns).await {
        Err(_) => {
            let _ = ctx
                .recorder()
                .publish_warning(
                    &ingress.object_ref(&()),
                    "NoPathRules",
                    "Ingress has no HTTP path rules; add spec.rules with at least one backend",
                )
                .await;
            deregister_and_cleanup(ctx, op_ns, &names, &ingress, &annotations.headscale_ref)
                .await?;
            return Ok(Action::await_change());
        }
        Ok(routes) if routes.is_empty() => {
            let _ = ctx
                .recorder()
                .publish_warning(
                    &ingress.object_ref(&()),
                    "Pending",
                    "backend Service not yet available; waiting for named port to resolve",
                )
                .await;
            return Ok(Action::requeue(Duration::from_secs(30)));
        }
        Ok(routes) => routes,
    };

    let mut headscale = headscale_connect(ctx, op_ns, &annotations.headscale_ref).await?;

    let wg_node_port = apply_wireguard_service(&child, &names).await?;

    if let AuthKeyStatus::WaitingForUser = ensure_auth_key(
        ctx,
        op_ns,
        &ingress,
        &mut headscale,
        &child,
        &names,
        annotations.user.as_deref(),
        &annotations.managed_key_tags,
        auto_tag.as_deref(),
        annotations.auth_key_expiry_secs,
        annotations.auth_key_reusable,
    )
    .await?
    {
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    let state_secret = ensure_state_secret(&child, &names, &annotations.headscale_ref).await?;

    apply_serve_configmap(&child, &names, &tailnet_fqdn, &routes, &cap_names).await?;

    apply_proxy_rbac(&child, &names).await?;

    apply_proxy_statefulset(
        &child,
        &names,
        &ctx.proxy_image,
        &internal_headscale_url,
        &annotations.hostname,
        wg_node_port,
    )
    .await?;

    let device_id = read_secret_string(&state_secret, "device_id");
    let device_ips =
        read_secret_json::<Vec<String>>(&state_secret, "device_ips").unwrap_or_default();

    // Keep the registered node's ACL tags in sync with the desired state on
    // every reconcile. ensure_auth_key returns early when the config secret
    // already exists, so without this call adding or removing access grants
    // after the proxy is registered would never update the node's tags.
    let mut set_tags_failed = false;
    if let Some(node_id) = device_id.as_ref().and_then(|s| s.parse::<u64>().ok()) {
        let desired_tags: Vec<String> = annotations
            .managed_key_tags
            .iter()
            .cloned()
            .chain(auto_tag.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if let Err(e) = headscale
            .set_tags(SetTagsRequest {
                node_id,
                tags: desired_tags,
            })
            .await
        {
            tracing::warn!(
                name = ingress_name,
                node_id,
                error = %e,
                "failed to set ACL tags on headscale node; will retry on next reconcile"
            );
            set_tags_failed = true;
        }
    }

    if device_id.is_some() {
        let tailnet_ip = device_ips.into_iter().next();
        if let Some(ref ip) = tailnet_ip {
            patch_ingress_status(ctx, &ingress, ip).await?;
            let recorder = ctx.recorder();
            let _ = recorder.publish_ready(&ingress.object_ref(&())).await;
        } else {
            tracing::info!(
                name = ingress_name,
                hostname = annotations.hostname,
                "Ingress: proxy registered but waiting for IP assignment"
            );
        }
    } else {
        tracing::info!(
            name = ingress_name,
            hostname = annotations.hostname,
            "Ingress: waiting for proxy to register"
        );
        let recorder = ctx.recorder();
        let _ = recorder
            .publish_warning(
                &ingress.object_ref(&()),
                "ProxyNotRegistered",
                &format!(
                    "proxy for Ingress '{ingress_name}' has not registered with headscale; \
                     if this persists beyond the auth-key expiry window, delete the \
                     Secret '{}' to force key rotation",
                    names.config_secret_name
                ),
            )
            .await;
    }

    if set_tags_failed {
        return Ok(Action::requeue(Duration::from_secs(30)));
    }
    Ok(Action::await_change())
}

// ── cleanup ───────────────────────────────────────────────────────────────────

async fn cleanup(ingress: Arc<Ingress>, ctx: &Context) -> Result<Action, Error> {
    let ingress_ns = ingress.namespace().unwrap_or_default();
    let ingress_name = ingress.name_any();
    let op_ns = &ctx.operator_namespace;
    let names = ProxyNames::new(&ingress_ns, &ingress_name);
    let headscale_ref_fallback = IngressAnnotations::headscale_ref(&ingress);
    deregister_and_cleanup(
        ctx,
        op_ns,
        &names,
        &ingress,
        headscale_ref_fallback.as_deref().unwrap_or(""),
    )
    .await?;
    Ok(Action::await_change())
}

/// Deregisters the proxy's headscale node (if registered) and deletes all proxy
/// k8s resources. Called on both Ingress deletion and namespace exclusion.
///
/// State secret read errors are propagated so the caller requeues and retries,
/// ensuring the node is removed before k8s resources are cleaned up. All other
/// errors (headscale connection, node deletion, k8s resource deletion) are
/// best-effort: logged or published as events, then cleanup continues.
async fn deregister_and_cleanup(
    ctx: &Context,
    op_ns: &str,
    names: &ProxyNames,
    ingress: &Ingress,
    headscale_ref_fallback: &str,
) -> Result<(), Error> {
    let ingress_name = ingress.name_any();

    // Read node_id and headscale_ref from the state Secret. On non-404 errors
    // we propagate and requeue — this retries until the API recovers, ensuring
    // the headscale node is deleted before k8s resources are cleaned up.
    let state_secret = match Api::<Secret>::namespaced(ctx.client.clone(), op_ns)
        .get(&names.state_secret_name)
        .await
    {
        Ok(secret) => Some(secret),
        Err(kube::Error::Api(ref e)) if e.code == 404 => None,
        Err(e) => return Err(Error::Kube(e)),
    };

    let node_id = state_secret
        .as_ref()
        .and_then(|s| read_secret_string(s, "device_id"))
        .and_then(|s| s.parse::<u64>().ok());

    if let Some(id) = node_id {
        let headscale_ref = state_secret
            .as_ref()
            .and_then(|s| read_secret_string(s, "headscale_ref"))
            .unwrap_or_else(|| headscale_ref_fallback.to_string());
        match headscale_connect(ctx, op_ns, &headscale_ref).await {
            Err(e) => {
                let recorder = ctx.recorder();
                let _ = recorder
                    .publish_warning(
                        &ingress.object_ref(&()),
                        "NodeOrphaned",
                        &format!(
                            "could not connect to headscale to delete node {id}: {e}; \
                             the node may remain registered in headscale"
                        ),
                    )
                    .await;
            }
            Ok(mut headscale) => {
                match headscale
                    .delete_node(DeleteNodeRequest { node_id: id })
                    .await
                {
                    Ok(_) => tracing::debug!(
                        name = ingress_name,
                        node_id = id,
                        "deleted node from headscale"
                    ),
                    Err(e) if e.code() == Code::NotFound => tracing::debug!(
                        name = ingress_name,
                        "cleanup: node already gone from headscale"
                    ),
                    Err(e) => {
                        // Return an error so the finalizer stays in place and
                        // the reconciler retries. The state Secret must not be
                        // deleted until we have confirmed headscale no longer
                        // tracks the node — it holds the node_id we need to
                        // retry the deletion.
                        tracing::warn!(
                            name = ingress_name,
                            node_id = id,
                            error = %e,
                            "cleanup: failed to delete node from headscale; will retry"
                        );
                        return Err(Error::HeadscaleApi(e));
                    }
                }
                let recorder = ctx.recorder();
                let _ = recorder.publish_deleted(&ingress.object_ref(&())).await;
            }
        }
    }

    cleanup_proxy_resources(ctx, op_ns, names).await;
    Ok(())
}

/// Explicitly deletes all proxy resources created in the operator namespace.
///
/// Proxy resources are owned by their HeadscaleInstance (same namespace), so GC
/// handles cleanup on HeadscaleInstance deletion. For Ingress deletion the owner
/// is still alive, so this explicit cleanup is still required.
/// All deletes are best-effort: 404s are silently ignored; unexpected errors are
/// logged so leaked resources are discoverable, but cleanup continues regardless.
async fn cleanup_proxy_resources(ctx: &Context, op_ns: &str, names: &ProxyNames) {
    let c = ctx.client.clone();
    tokio::join!(
        del_warn(
            Api::<StatefulSet>::namespaced(c.clone(), op_ns),
            &names.proxy_name
        ),
        del_warn(
            Api::<Service>::namespaced(c.clone(), op_ns),
            &names.wg_service_name
        ),
        del_warn(
            Api::<Secret>::namespaced(c.clone(), op_ns),
            &names.config_secret_name
        ),
        del_warn(
            Api::<Secret>::namespaced(c.clone(), op_ns),
            &names.state_secret_name
        ),
        del_warn(
            Api::<ConfigMap>::namespaced(c.clone(), op_ns),
            &names.serve_configmap_name
        ),
        del_warn(
            Api::<RoleBinding>::namespaced(c.clone(), op_ns),
            &names.proxy_name
        ),
        del_warn(Api::<Role>::namespaced(c.clone(), op_ns), &names.proxy_name),
        del_warn(
            Api::<ServiceAccount>::namespaced(c, op_ns),
            &names.proxy_name
        ),
    );
}

/// Best-effort delete used by `cleanup_proxy_resources`: 404 is success,
/// any other error is logged and swallowed so the parallel cleanup of the
/// remaining resources continues.
async fn del_warn<K>(api: Api<K>, name: &str)
where
    K: Resource + serde::de::DeserializeOwned + Clone + std::fmt::Debug,
{
    if let Err(e) = delete_ignoring_404(api, name).await {
        tracing::warn!(resource = name, error = %e, "cleanup: failed to delete proxy resource");
    }
}

// ── headscale connection ──────────────────────────────────────────────────────

pub(crate) async fn headscale_connect(
    ctx: &Context,
    namespace: &str,
    name: &str,
) -> Result<AuthenticatedClient, kube::Error> {
    let secret_name = format!("headscale-api-key-{name}");
    let api_key = Api::<Secret>::namespaced(ctx.client.clone(), namespace)
        .get(&secret_name)
        .await
        .map_err(|e| match e {
            kube::Error::Api(ref ae) if ae.code == 404 => kube::Error::Api(Box::new(
                kube::error::Status::failure(
                    &format!("Secret {secret_name} not found; is HeadscaleInstance ready?"),
                    "NotFound",
                )
                .with_code(404),
            )),
            other => other,
        })?
        .data
        .as_ref()
        .and_then(|d| d.get("HEADSCALE_API_KEY"))
        .map(|b| String::from_utf8_lossy(&b.0).into_owned())
        .ok_or_else(|| {
            kube::Error::Api(Box::new(
                kube::error::Status::failure(
                    "api-key secret has no 'HEADSCALE_API_KEY' field",
                    "InvalidSecret",
                )
                .with_code(500),
            ))
        })?;
    ctx.headscale
        .connect(
            &format!("http://headscale-server-{name}.{namespace}.svc:50443"),
            &api_key,
        )
        .await
        .map_err(|e| kube::Error::Service(Box::new(e)))
}

// ── namespace helper ──────────────────────────────────────────────────────────

async fn namespace_is_deleting(client: &Client, ns: &str) -> Result<bool, Error> {
    match Api::<Namespace>::all(client.clone()).get(ns).await {
        Ok(ns_obj) => Ok(ns_obj.metadata.deletion_timestamp.is_some()),
        Err(kube::Error::Api(ref e)) if e.code == 404 => Ok(true),
        Err(e) => Err(Error::Kube(e)),
    }
}

// ── secret helpers ────────────────────────────────────────────────────────────

fn read_secret_string(secret: &Secret, key: &str) -> Option<String> {
    String::from_utf8(secret.item(key)?.0.clone()).ok()
}

fn read_secret_json<T: serde::de::DeserializeOwned>(secret: &Secret, key: &str) -> Option<T> {
    serde_json::from_slice(&secret.item(key)?.0).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::ingress::test_support::{headmaster_ingress, test_ctx, test_ingress};
    use crate::test_support::{FaultService, all_404, all_500};

    // ── namespace_is_deleting tests ───────────────────────────────────────────

    #[tokio::test]
    async fn namespace_is_deleting_returns_true_on_404() {
        let client = FaultService::client(all_404);
        let result = namespace_is_deleting(&client, "gone-ns").await.unwrap();
        assert!(
            result,
            "404 on namespace GET must be treated as namespace gone (deleting)"
        );
    }

    #[tokio::test]
    async fn namespace_is_deleting_propagates_non_404_error() {
        let client = FaultService::client(all_500);
        let result = namespace_is_deleting(&client, "any-ns").await;
        assert!(result.is_err(), "non-404 GET error must propagate");
    }

    // ── deregister_and_cleanup tests ──────────────────────────────────────────

    #[tokio::test]
    async fn deregister_and_cleanup_propagates_state_secret_error() {
        let (k8s, calls) = FaultService::tracked(all_500);
        let ctx = test_ctx(k8s);
        let names = ProxyNames::new("default", "test-ingress");

        let result = deregister_and_cleanup(&ctx, "default", &names, &test_ingress(), "main").await;

        assert!(result.is_err(), "state-secret GET error must propagate");
        let recorded = calls.lock().unwrap();
        assert!(
            recorded.iter().all(|(m, _)| m == "GET"),
            "no DELETE calls must be issued when state-secret read fails: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn deregister_and_cleanup_continues_when_state_secret_absent() {
        // all_404: state-secret GET → 404 (None), proxy resource DELETEs → 404 (silently ignored).
        let ctx = test_ctx(FaultService::client(all_404));
        let names = ProxyNames::new("default", "test-ingress");

        let result = deregister_and_cleanup(&ctx, "default", &names, &test_ingress(), "main").await;

        assert!(
            result.is_ok(),
            "missing state secret must not abort cleanup"
        );
    }

    // ── ingress_watch_namespaces tests ────────────────────────────────────────

    #[tokio::test]
    async fn reconcile_skips_ingress_in_unwatched_namespace() {
        // all_500 means any K8s call would fail; if reconcile reaches the
        // finalizer it returns Err. Returning Ok proves the namespace filter fired.
        let ctx = Arc::new(Context {
            ingress_watch_namespaces: vec!["prod".to_string()],
            ..test_ctx(FaultService::client(all_500))
        });
        let result = super::reconcile(Arc::new(headmaster_ingress("staging")), ctx).await;
        assert!(
            result.is_ok(),
            "ingress in a non-watched namespace must be silently skipped"
        );
    }

    #[tokio::test]
    async fn reconcile_processes_ingress_in_watched_namespace() {
        // Ingress IS in the watched namespace, so reconcile proceeds past the
        // filter and reaches the K8s finalizer call, which fails with 500.
        let ctx = Arc::new(Context {
            ingress_watch_namespaces: vec!["prod".to_string()],
            ..test_ctx(FaultService::client(all_500))
        });
        let result = super::reconcile(Arc::new(headmaster_ingress("prod")), ctx).await;
        assert!(
            result.is_err(),
            "ingress in a watched namespace must be processed (K8s call expected)"
        );
    }

    #[tokio::test]
    async fn reconcile_processes_all_namespaces_when_watch_list_empty() {
        // Empty watch list = watch all. Reconcile proceeds and hits the K8s
        // finalizer call, which fails with 500.
        let ctx = Arc::new(test_ctx(FaultService::client(all_500)));
        let result = super::reconcile(Arc::new(headmaster_ingress("any-namespace")), ctx).await;
        assert!(
            result.is_err(),
            "with an empty watch list all namespaces must be processed"
        );
    }

    // ── watchedNamespaces finalizer removal tests ─────────────────────────────

    fn instance_with_watched_namespaces_responder(m: &http::Method, path: &str) -> (u16, Vec<u8>) {
        use crate::types::{
            HeadscaleInstance, HeadscaleInstanceSpec, HeadscaleInstanceStatus, StorageSpec,
        };
        use k8s_openapi::api::core::v1::Namespace as K8sNamespace;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

        if *m == http::Method::GET && path.contains("headscaleinstances") {
            let instance = HeadscaleInstance {
                metadata: ObjectMeta {
                    name: Some("main".to_string()),
                    namespace: Some("default".to_string()),
                    uid: Some("uid-1".to_string()),
                    ..Default::default()
                },
                spec: HeadscaleInstanceSpec {
                    server_url: "https://headscale.example.com".to_string(),
                    dns_base_domain: "ts.example.com".to_string(),
                    storage: StorageSpec {
                        size: "1Gi".to_string(),
                        ..Default::default()
                    },
                    watched_namespaces: vec!["prod".to_string()],
                    ..Default::default()
                },
                status: Some(HeadscaleInstanceStatus {
                    conditions: vec![Condition {
                        type_: "Ready".to_string(),
                        status: "True".to_string(),
                        reason: "Ready".to_string(),
                        message: "ready".to_string(),
                        last_transition_time: Time(k8s_openapi::jiff::Timestamp::UNIX_EPOCH),
                        observed_generation: None,
                    }],
                    ..Default::default()
                }),
            };
            (200, serde_json::to_vec(&instance).unwrap())
        } else if *m == http::Method::GET && path.contains("/namespaces/staging") {
            // Return a live namespace without deletionTimestamp so namespace_is_deleting returns false.
            let ns = K8sNamespace {
                metadata: ObjectMeta {
                    name: Some("staging".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            };
            (200, serde_json::to_vec(&ns).unwrap())
        } else if *m == http::Method::PATCH {
            // Return a plausible Ingress JSON for the status clear and finalizer patch.
            (200, serde_json::json!({
                "apiVersion": "networking.k8s.io/v1",
                "kind": "Ingress",
                "metadata": {"name": "test-ingress", "namespace": "staging", "resourceVersion": "2"}
            }).to_string().into_bytes())
        } else {
            // GETs for secrets, statefulsets, etc. — 404 so cleanup proceeds cleanly.
            (404, br#"{"code":404}"#.to_vec())
        }
    }

    #[tokio::test]
    async fn apply_removes_finalizer_when_namespace_excluded_from_watched_namespaces() {
        use crate::controllers::ingress::ANNOTATION_CONFIG;
        use k8s_openapi::api::networking::v1::IngressSpec;
        use std::collections::BTreeMap;

        let ingress = Arc::new(Ingress {
            metadata: ObjectMeta {
                name: Some("test-ingress".to_string()),
                namespace: Some("staging".to_string()),
                uid: Some("uid-ing-1".to_string()),
                finalizers: Some(vec![FINALIZER.to_string()]),
                annotations: Some(BTreeMap::from([(
                    ANNOTATION_CONFIG.to_string(),
                    r#"{"headscale-ref":"main","user":"alice"}"#.to_string(),
                )])),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                ingress_class_name: Some(INGRESS_CLASS_NAME.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });

        let (k8s, calls) = FaultService::tracked(instance_with_watched_namespaces_responder);
        let ctx = test_ctx(k8s);

        let result = apply(ingress, &ctx).await;
        assert!(
            result.is_ok(),
            "apply must succeed when namespace is excluded from watchedNamespaces"
        );

        let recorded = calls.lock().unwrap();
        let has_ingress_patch = recorded
            .iter()
            .any(|(m, p)| m == "PATCH" && p.contains("/ingresses/test-ingress"));
        assert!(
            has_ingress_patch,
            "a PATCH to remove the finalizer must be issued: {recorded:?}"
        );
    }
}
