use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PodSecurityContext, PodSpec, PodTemplateSpec, Probe, SeccompProfile, Secret, SecretEnvSource,
    Service, ServicePort, ServiceSpec, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi_ext::{
    ContainerExt, ContainerPortExt, EnvVarExt, PodSpecExt, PodTemplateSpecExt, ProbeExt, SecretExt,
    ServiceExt, ServicePortExt, StatefulSetExt,
};
use kube::api::Api;
use rand::RngCore;

use super::Error;
use crate::context::Context;
use crate::controllers::applier::{ChildApplier, delete_ignoring_404};
use crate::types::ScimSpec;

/// Ensures the SCIM StatefulSet, Service, and bearer-token Secret exist for the instance.
pub(super) async fn ensure_scim(
    ctx: &Context,
    child: &ChildApplier<'_>,
    scim: &ScimSpec,
) -> Result<(), Error> {
    ensure_scim_token(ctx, child).await?;

    let instance = &child.instance;
    let ns = &child.namespace;
    let scim_name = format!("headscale-scim-{instance}");

    let mut scim_env = vec![EnvVar::value(
        "HEADSCALE_URL",
        format!("http://headscale-server-{instance}.{ns}.svc:50443"),
    )];
    if let Some(key) = &scim.policy_user_key {
        scim_env.push(EnvVar::value("POLICY_USER_KEY", key.clone()));
    }
    if let Some(issuer) = &scim.oidc_issuer {
        scim_env.push(EnvVar::value("OIDC_ISSUER", issuer.clone()));
    }
    if scim.expire_nodes_on_change {
        scim_env.push(EnvVar::value("EXPIRE_NODES_ON_CHANGE", "true"));
    }

    let scim_container = Container::new("scim")
        .image(&ctx.operator_image)
        .command(["/usr/local/bin/headmaster-scim"])
        .allow_privilege_escalation(false)
        .read_only_root_filesystem(true)
        .drop_capabilities(["ALL"])
        .env(scim_env)
        .env_from([
            SecretEnvSource {
                name: format!("headscale-api-key-{instance}"),
                optional: Some(false),
            },
            SecretEnvSource {
                name: format!("headscale-scim-token-{instance}"),
                optional: Some(false),
            },
        ])
        .ports([ContainerPort::tcp(8081).name("scim")])
        .volume_mounts([VolumeMount {
            name: "data".into(),
            mount_path: "/data".into(),
            ..Default::default()
        }])
        .readiness_probe(
            Probe::http_get("/readyz", "scim")
                .initial_delay_seconds(5)
                .period_seconds(5)
                .failure_threshold(3),
        )
        .liveness_probe(
            Probe::http_get("/livez", "scim")
                .initial_delay_seconds(30)
                .period_seconds(10)
                .failure_threshold(3),
        );

    let scim_pod_spec = PodSpec {
        security_context: Some(PodSecurityContext {
            run_as_non_root: Some(true),
            run_as_user: Some(65532),
            run_as_group: Some(65532),
            fs_group: Some(65532),
            seccomp_profile: Some(SeccompProfile {
                type_: "RuntimeDefault".into(),
                localhost_profile: None,
            }),
            ..Default::default()
        }),
        ..PodSpec::container(scim_container)
    };

    let scim_pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some("data".into()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".into()]),
            storage_class_name: scim.storage.storage_class.clone(),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".into(),
                    Quantity(scim.storage.size.clone()),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    child
        .apply_statefulset(
            "scim",
            StatefulSet::new(&scim_name)
                .replicas(1)
                .service_name(&scim_name)
                .template(PodTemplateSpec::new().pod_spec(scim_pod_spec))
                .volume_claim_templates([scim_pvc]),
        )
        .await?;

    child
        .apply_service(
            "scim",
            Service::new(&scim_name).spec(ServiceSpec {
                ports: Some(vec![ServicePort::tcp("scim", 8081_i32).target_port("scim")]),
                ..Default::default()
            }),
        )
        .await?;

    Ok(())
}

async fn ensure_scim_token(ctx: &Context, child: &ChildApplier<'_>) -> Result<(), Error> {
    let secret_name = format!("headscale-scim-token-{}", child.instance);
    let secret_api = Api::<Secret>::namespaced(ctx.client.clone(), &child.namespace);

    if secret_api.get_opt(&secret_name).await?.is_some() {
        return Ok(());
    }

    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);

    tracing::info!(
        name = child.instance,
        "HeadscaleInstance: generated SCIM bearer token"
    );

    child
        .apply(
            "scim",
            Secret::new(&secret_name).string_data([("SCIM_BEARER_TOKEN", token)]),
        )
        .await?;
    Ok(())
}

pub(super) async fn delete_scim_if_exists(
    ctx: &Context,
    ns: &str,
    instance: &str,
) -> Result<(), Error> {
    let scim_name = format!("headscale-scim-{instance}");
    let secret_name = format!("headscale-scim-token-{instance}");
    // TODO: remove explicit PVC deletion once k3s PVC retention policy bug is fixed;
    // replace with persistentVolumeClaimRetentionPolicy whenDeleted=Delete on the StatefulSet.
    let scim_pvc_name = format!("data-headscale-scim-{instance}-0");

    let c = &ctx.client;
    delete_ignoring_404(Api::<StatefulSet>::namespaced(c.clone(), ns), &scim_name).await?;
    delete_ignoring_404(Api::<Service>::namespaced(c.clone(), ns), &scim_name).await?;
    delete_ignoring_404(Api::<Secret>::namespaced(c.clone(), ns), &secret_name).await?;
    delete_ignoring_404(
        Api::<PersistentVolumeClaim>::namespaced(c.clone(), ns),
        &scim_pvc_name,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::headscale_instance::test_support::test_ctx;
    use crate::test_support::{FaultService, all_404};

    #[tokio::test]
    async fn delete_scim_if_exists_ignores_404_on_all_resources() {
        // All DELETE calls return 404 (resources already absent). The macro in
        // delete_scim_if_exists silently ignores 404, so the function must return Ok(()).
        let ctx = test_ctx(FaultService::client(all_404));
        let result = delete_scim_if_exists(&ctx, "default", "my-instance").await;
        assert!(
            result.is_ok(),
            "404 on every DELETE must be silently ignored"
        );
    }

    fn sts_ok_service_fails(m: &http::Method, path: &str) -> (u16, Vec<u8>) {
        if *m == http::Method::DELETE && path.contains("/services/") {
            (500, br#"{"code":500}"#.to_vec())
        } else {
            // StatefulSet DELETE and everything else → 404 (silently ok in the macro).
            (404, br#"{"code":404}"#.to_vec())
        }
    }

    #[tokio::test]
    async fn delete_scim_if_exists_propagates_mid_sequence_error() {
        // StatefulSet DELETE → 404 (ok), Service DELETE → 500 (fail).
        // The function must propagate the error and must NOT proceed to delete
        // the Secret or PVC.
        let (k8s, calls) = FaultService::tracked(sts_ok_service_fails);
        let ctx = test_ctx(k8s);

        let result = delete_scim_if_exists(&ctx, "default", "my-instance").await;

        assert!(result.is_err(), "mid-sequence 500 must propagate");

        let recorded = calls.lock().unwrap();
        let paths: Vec<&str> = recorded.iter().map(|(_, p)| p.as_str()).collect();
        assert!(
            !paths.iter().any(|p| p.contains("/secrets/")),
            "Secret DELETE must not be issued after Service DELETE fails: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.contains("/persistentvolumeclaims/")),
            "PVC DELETE must not be issued after Service DELETE fails: {paths:?}"
        );
    }
}
