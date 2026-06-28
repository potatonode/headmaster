use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use headscale_client::LiveConnector;
use headscale_client::fake::{FakeConnector, FakeHeadscaleServer};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::api::{DeleteParams, PostParams};
use operator::controllers::{headscale_instance, ingress};
use operator::types::{HeadscaleInstance, HeadscaleInstanceSpec, StorageSpec};

use super::support::{make_ctx, make_ingress};
use super::{client, unique_ns};

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn headscale_instance_creates_child_resources() {
    let kube_client = client().await;
    let ns = unique_ns(&kube_client).await;

    let ctx = make_ctx(&kube_client, &ns, Arc::new(LiveConnector));

    let handle = tokio::spawn(headscale_instance::stream(
        kube::Api::namespaced(kube_client.clone(), &ns),
        ctx,
        std::future::pending(),
    ));

    Api::<HeadscaleInstance>::namespaced(kube_client.clone(), &ns)
        .create(
            &PostParams::default(),
            &HeadscaleInstance {
                metadata: ObjectMeta {
                    name: Some("hs".to_string()),
                    namespace: Some(ns.clone()),
                    ..Default::default()
                },
                spec: HeadscaleInstanceSpec {
                    server_url: "https://headscale.example.com".to_string(),
                    dns_base_domain: "ts.example.com".to_string(),
                    storage: StorageSpec {
                        size: "1Gi".to_string(),
                        ..Default::default()
                    },
                    extra_config: BTreeMap::from([
                        (
                            "derp".to_string(),
                            serde_json::json!({
                                "urls": ["https://controlplane.tailscale.com/derpmap/default"],
                                "auto_update_enabled": true,
                                "update_frequency": "24h",
                            }),
                        ),
                        (
                            "prefixes".to_string(),
                            serde_json::json!({
                                "v4": "100.64.0.0/10",
                                "v6": "fd7a:115c:a1e0::/48",
                                "allocation": "sequential"
                            }),
                        ),
                    ]),
                    ..Default::default()
                },
                status: None,
            },
        )
        .await
        .expect("create HeadscaleInstance");

    // Wait up to 10 s for all three child resources to appear.
    let cm_api = Api::<ConfigMap>::namespaced(kube_client.clone(), &ns);
    let svc_api = Api::<Service>::namespaced(kube_client.clone(), &ns);
    let sts_api = Api::<StatefulSet>::namespaced(kube_client.clone(), &ns);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let all_present = cm_api.get("headscale-server-hs").await.is_ok()
            && svc_api.get("headscale-server-hs").await.is_ok()
            && sts_api.get("headscale-server-hs").await.is_ok();
        if all_present {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: child resources did not appear within 10s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    handle.abort();
}

#[tokio::test]
async fn headscale_instance_deletion_orphans_referencing_ingresses() {
    let kube_client = client().await;
    let ns = unique_ns(&kube_client).await;

    let connector = Arc::new(FakeConnector::new(FakeHeadscaleServer::default()).await);
    let ctx = make_ctx(&kube_client, &ns, connector);

    let hi_handle = tokio::spawn(headscale_instance::stream(
        Api::namespaced(kube_client.clone(), &ns),
        ctx.clone(),
        std::future::pending(),
    ));
    let ingress_handle = tokio::spawn(ingress::stream(
        Api::namespaced(kube_client.clone(), &ns),
        ctx.clone(),
        std::future::pending(),
    ));

    let hi_api: Api<HeadscaleInstance> = Api::namespaced(kube_client.clone(), &ns);
    hi_api
        .create(
            &PostParams::default(),
            &HeadscaleInstance {
                metadata: ObjectMeta {
                    name: Some("hs".to_string()),
                    namespace: Some(ns.clone()),
                    ..Default::default()
                },
                spec: HeadscaleInstanceSpec {
                    server_url: "https://headscale.example.com".to_string(),
                    dns_base_domain: "ts.example.com".to_string(),
                    storage: StorageSpec {
                        size: "1Gi".to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                status: None,
            },
        )
        .await
        .expect("create HeadscaleInstance");

    // Wait for the controller to add its finalizer.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let hi = hi_api.get("hs").await.expect("get HeadscaleInstance");
        if hi
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|f| !f.is_empty())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for HeadscaleInstance finalizer"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let ingress_api: Api<Ingress> = Api::namespaced(kube_client.clone(), &ns);
    ingress_api
        .create(
            &PostParams::default(),
            &make_ingress("test-ingress", &ns, "hs"),
        )
        .await
        .expect("create Ingress");

    // Wait for the ingress controller to reconcile the Ingress (adds its finalizer),
    // which confirms it is visible in the list cache before we trigger cleanup.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ing = ingress_api.get("test-ingress").await.expect("get Ingress");
        if ing
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|f| !f.is_empty())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for Ingress finalizer"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Delete the HeadscaleInstance — should complete without blocking on the Ingress.
    hi_api
        .delete("hs", &DeleteParams::default())
        .await
        .expect("delete HeadscaleInstance");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match hi_api.get("hs").await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => break,
            Err(e) => panic!("unexpected error waiting for HeadscaleInstance deletion: {e}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: HeadscaleInstance was not deleted despite no blocking"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Ingress must still exist — it was orphaned, not deleted.
    ingress_api
        .get("test-ingress")
        .await
        .expect("Ingress must still exist after HeadscaleInstance deletion");

    hi_handle.abort();
    ingress_handle.abort();
}
