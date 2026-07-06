//! Main reconcile loop for `HeadscaleInstance`. Drives the apply/cleanup
//! lifecycle: ensures the headscale StatefulSet, Service, ConfigMap, API-key
//! Secret, optional SCIM sidecar, and policy are all in the desired state.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_ext::{ServiceExt, ServicePortExt, StatefulSetGetExt};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    ConfigMap, PersistentVolumeClaim, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::api::networking::v1::Ingress;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Resource, ResourceExt};

use super::bootstrap::ensure_api_key;
use super::builders::{build_configmap, desired_statefulset};
use super::policy::{policy_has_groups_with_members, sync_policy};
use super::scim::{delete_scim_if_exists, ensure_scim};
use super::{Error, PORT_GRPC, PORT_HTTP, PORT_METRICS};
use crate::context::Context;
use crate::controllers::applier::{Applier, ChildApplier, delete_ignoring_404};
use crate::controllers::recorder::RecorderExt;
use crate::types::{HeadscaleInstance, IngressAnnotations, ResourceStatus};
use crate::{FIELD_MANAGER, FINALIZER, labels};

/// Runs the `HeadscaleInstance` controller until `shutdown` resolves.
pub fn stream(
    api: Api<HeadscaleInstance>,
    ctx: Arc<Context>,
    shutdown: impl Future<Output = ()> + Send + Sync + 'static,
) -> impl Future<Output = ()> {
    let ns = api
        .namespace()
        .expect("HeadscaleInstance API must be namespaced")
        .to_owned();
    let owns_cfg = watcher::Config::default().labels(&labels::managed_by_selector());

    kube::runtime::Controller::new(api, Default::default())
        .owns(
            Api::<StatefulSet>::namespaced(ctx.client.clone(), &ns),
            owns_cfg.clone(),
        )
        .owns(
            Api::<ConfigMap>::namespaced(ctx.client.clone(), &ns),
            owns_cfg.clone(),
        )
        .owns(
            Api::<Service>::namespaced(ctx.client.clone(), &ns),
            owns_cfg,
        )
        .watches(
            Api::<Ingress>::all(ctx.client.clone()),
            watcher::Config::default(),
            {
                let op_ns = ns.clone();
                move |ing| {
                    IngressAnnotations::headscale_ref(&ing)
                        .map(|href| {
                            ObjectRef::<HeadscaleInstance>::new(href.as_str()).within(&op_ns)
                        })
                        .into_iter()
                }
            },
        )
        .graceful_shutdown_on(shutdown)
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::warn!(error = ?e, "HeadscaleInstance reconcile error");
            }
        })
}

async fn reconcile(obj: Arc<HeadscaleInstance>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = obj.namespace().ok_or(Error::MissingNamespace)?;
    let api: Api<HeadscaleInstance> = Api::namespaced(ctx.client.clone(), &ns);
    finalizer(&api, FINALIZER, obj, |event| async {
        match event {
            Finalizer::Apply(obj) => apply(obj, &ctx).await,
            Finalizer::Cleanup(obj) => cleanup(obj, &ctx).await,
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

fn error_policy(_obj: Arc<HeadscaleInstance>, e: &Error, _ctx: Arc<Context>) -> Action {
    tracing::warn!("HeadscaleInstance reconcile failed: {e:?}");
    Action::requeue(Duration::from_secs(30))
}

async fn apply(obj: Arc<HeadscaleInstance>, ctx: &Context) -> Result<Action, Error> {
    let ns = obj.namespace().ok_or(Error::MissingNamespace)?;
    let name = obj.name_any();
    let client = &ctx.client;

    let old_status = obj.status.clone().unwrap_or_default();
    let generation = obj.metadata.generation.unwrap_or(0);

    let child = ChildApplier::from_parent(ctx, &obj);
    let headscale_name = format!("headscale-server-{name}");

    let a = Applier::from_ctx(ctx);

    if let Err(e) = ensure_headscale(ctx, &child, &obj).await {
        let mut error_status = old_status.clone();
        error_status.update_ready(
            false,
            "ChildApplyFailed",
            format!("failed to apply child resource: {e}"),
            generation,
        );
        let _ = a.apply_status(&*obj, &error_status).await;
        return Err(e);
    }

    let live_sts = Api::<StatefulSet>::namespaced(client.clone(), &ns)
        .get(&headscale_name)
        .await?;
    let is_ready = live_sts.ready_replicas().unwrap_or(0) > 0;

    if !is_ready {
        let mut new_status = old_status.clone();
        new_status.update_ready(
            false,
            "StatefulSetNotReady",
            "headscale StatefulSet is not yet ready",
            generation,
        );
        a.apply_status(&*obj, &new_status).await?;
        let obj_ref = obj.object_ref(&());
        ctx.recorder()
            .publish_transitions(&old_status, &new_status, &obj_ref)
            .await;
        return Ok(Action::requeue(Duration::from_secs(10)));
    }

    // Run all post-readiness operations in a single block so that any failure
    // is caught once and reflected in the status before propagating. Without
    // this, a failure here after a previously-successful reconcile leaves the
    // status stale at Ready=True while the reconciler loops on error.
    let result: Result<(), Error> = async {
        ensure_api_key(ctx, &child).await?;

        // The webhook blocks this at admission, but guard again here in case it
        // is bypassed: SCIM owns the groups section; a non-empty groups key in
        // spec.policy.inline would be clobbered by sync_policy's full replacement.
        if obj.spec.scim.is_some() && policy_has_groups_with_members(obj.spec.policy.as_ref()) {
            return Err(Error::ScimPolicyConflict);
        }
        let contributing_ingresses =
            list_contributing_ingresses(&ctx.client, &name, &obj.spec.watched_namespaces).await?;
        sync_policy(
            ctx,
            &ns,
            &name,
            obj.spec.policy.as_ref(),
            obj.spec.scim.is_some(),
            &contributing_ingresses,
        )
        .await?;

        match &obj.spec.scim {
            Some(scim) => ensure_scim(ctx, &child, scim).await,
            None => delete_scim_if_exists(ctx, &ns, &name).await,
        }
    }
    .await;

    if let Err(e) = result {
        let reason = match &e {
            Error::ScimPolicyConflict => "ScimPolicyConflict",
            _ => "ReconcileFailed",
        };
        let mut error_status = old_status.clone();
        error_status.update_ready(false, reason, e.to_string(), generation);
        let _ = a.apply_status(&*obj, &error_status).await;
        return Err(e);
    }

    let mut new_status = old_status.clone();
    new_status.update_ready(
        true,
        "StatefulSetReady",
        "headscale StatefulSet is ready",
        generation,
    );
    a.apply_status(&*obj, &new_status).await?;
    let obj_ref = obj.object_ref(&());
    ctx.recorder()
        .publish_transitions(&old_status, &new_status, &obj_ref)
        .await;

    // Periodic requeue so that WaitingForGroup grants are retried after SCIM
    // syncs new groups to headscale. SCIM is k8s-agnostic and does not touch
    // any watched resource, so watch events alone are not sufficient.
    Ok(Action::requeue(Duration::from_secs(60)))
}

/// Cleans up a `HeadscaleInstance` before the finalizer is removed.
///
/// Ingresses that still reference this instance are orphaned: their
/// `status.loadBalancer.ingress` is cleared and a warning event is posted so
/// operators can see what happened. The Ingress controller will keep requeueing
/// them and publishing "Pending" events until the user re-points or deletes them.
/// Built-in children (StatefulSet, ConfigMap, Service, Secret) are operator-owned
/// via ownerReferences and are garbage-collected automatically.
/// PVCs from volumeClaimTemplates are NOT garbage-collected automatically; see
/// the explicit deletion block below.
async fn cleanup(obj: Arc<HeadscaleInstance>, ctx: &Context) -> Result<Action, Error> {
    let instance_name = obj.name_any();

    let referencing = list_contributing_ingresses(&ctx.client, &instance_name, &[]).await?;

    let recorder = ctx.recorder();
    let ssa = PatchParams::apply(FIELD_MANAGER).force();
    for ing in &referencing {
        let ing_ns = ing.namespace().unwrap_or_default();
        let ing_name = ing.name_any();
        let _ = recorder
            .publish_warning(
                &ing.object_ref(&()),
                "InstanceDeleted",
                &format!(
                    "HeadscaleInstance '{instance_name}' was deleted; \
                     this Ingress is now orphaned and will stop functioning"
                ),
            )
            .await;
        let _ = Api::<Ingress>::namespaced(ctx.client.clone(), &ing_ns)
            .patch_status(
                &ing_name,
                &ssa,
                &Patch::Apply(serde_json::json!({
                    "apiVersion": "networking.k8s.io/v1",
                    "kind": "Ingress",
                    "metadata": { "name": ing_name, "namespace": ing_ns },
                    "status": {}
                })),
            )
            .await;
    }

    if !referencing.is_empty() {
        tracing::info!(
            name = instance_name,
            count = referencing.len(),
            "HeadscaleInstance cleanup: orphaned referencing Ingresses"
        );
    }

    tracing::info!(
        name = instance_name,
        "HeadscaleInstance cleanup: proceeding"
    );

    // Explicitly delete PVCs created from volumeClaimTemplates. Kubernetes does
    // not garbage-collect these automatically because they have no ownerReference
    // to the HeadscaleInstance.
    //
    // TODO: remove this block and set persistentVolumeClaimRetentionPolicy
    // whenDeleted=Delete on both StatefulSets once k3s fixes the bug where that
    // policy prevents readyReplicas from being updated (k3s 1.32.5).
    let ns = obj.namespace().ok_or(Error::MissingNamespace)?;
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
    for pvc_name in [
        format!("data-headscale-server-{instance_name}-0"),
        format!("data-headscale-scim-{instance_name}-0"),
    ] {
        delete_ignoring_404(pvc_api.clone(), &pvc_name).await?;
    }

    let _ = ctx.recorder().publish_deleted(&obj.object_ref(&())).await;
    Ok(Action::await_change())
}

/// Lists all Ingresses across all namespaces that reference the named
/// HeadscaleInstance. Used to enumerate contributing Ingresses for policy grants.
async fn list_contributing_ingresses(
    client: &kube::Client,
    instance_name: &str,
    watched_namespaces: &[String],
) -> Result<Vec<Ingress>, Error> {
    let ingress_api = Api::<Ingress>::all(client.clone());
    let all_ingresses = ingress_api
        .list(&ListParams::default())
        .await
        .map_err(Error::Kube)?
        .items;
    Ok(all_ingresses
        .into_iter()
        .filter(|ing| IngressAnnotations::headscale_ref(ing).as_deref() == Some(instance_name))
        .filter(|ing| {
            watched_namespaces.is_empty()
                || watched_namespaces.contains(&ing.namespace().unwrap_or_default())
        })
        .collect())
}

async fn ensure_headscale(
    ctx: &Context,
    child: &ChildApplier<'_>,
    obj: &HeadscaleInstance,
) -> Result<(), Error> {
    let headscale_name = format!("headscale-server-{}", child.instance);
    let (config_map, hash) = build_configmap(
        &headscale_name,
        &obj.spec.server_url,
        &obj.spec.dns_base_domain,
        &obj.spec.extra_config,
    )?;
    child.apply("headscale", config_map).await?;
    child
        .apply_service(
            "headscale",
            Service::new(&headscale_name).spec(ServiceSpec {
                ports: Some(vec![
                    ServicePort::tcp("http", PORT_HTTP).target_port("http"),
                    ServicePort::tcp("metrics", PORT_METRICS).target_port("metrics"),
                    ServicePort::tcp("grpc", PORT_GRPC).target_port("grpc"),
                ]),
                ..Default::default()
            }),
        )
        .await?;
    child
        .apply_statefulset(
            "headscale",
            desired_statefulset(
                &headscale_name,
                &ctx.headscale_image,
                &obj.spec.storage,
                obj.spec.resources.as_ref(),
                &hash,
            ),
        )
        .await?;
    Ok(())
}
