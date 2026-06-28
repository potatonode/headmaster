use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Container, EnvVar, Pod, PodSpec, Volume, VolumeMount};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::ResourceExt;
use kube::api::{DeleteParams, LogParams, PostParams};
use operator::types::HeadscaleInstance;
use tokio::sync::OnceCell;

use super::{client, config};

// ── setup ─────────────────────────────────────────────────────────────────────

struct ExamplesSetup {
    /// Hostname extracted from the examples Helm chart's serverUrl value.
    server_hostnames: Vec<String>,
}

// Stores a Result so a failed setup run is not retried by each waiting test.
// If stored as bare Arc and the init closure panics, the OnceCell resets and
// every parallel test reruns do_setup() from scratch, adding 5+ minutes each.
static SETUP: OnceCell<Result<Arc<ExamplesSetup>, String>> = OnceCell::const_new();

async fn setup() -> Arc<ExamplesSetup> {
    SETUP
        .get_or_init(|| async { do_setup().await.map(Arc::new).map_err(|e| e.to_string()) })
        .await
        .as_ref()
        .unwrap_or_else(|e| panic!("e2e examples setup failed: {e}"))
        .clone()
}

// Extracts the headscale hostname from the HEADSCALE_SERVER_URL env var so
// tests know which hostname the ingress is serving. Cluster setup (CoreDNS
// alias, resource install, readiness wait) is handled by `task setup-examples`
// before the test binary starts.
async fn do_setup() -> Result<ExamplesSetup, Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let hostname = hostname_from_url(&config().headscale_server_url)?.to_string();

    Ok(ExamplesSetup {
        server_hostnames: vec![hostname],
    })
}

fn hostname_from_url(url: &str) -> Result<&str, Box<dyn std::error::Error>> {
    url.split_once("://")
        .map(|(_, rest)| {
            let host_and_port = rest.split('/').next().unwrap_or(rest);
            host_and_port.split(':').next().unwrap_or(host_and_port)
        })
        .ok_or_else(|| format!("cannot extract hostname from {url:?}").into())
}

// ── tailnet connectivity helpers ──────────────────────────────────────────────

async fn container_logs_for_pod(
    kube_client: &kube::Client,
    pod_name: &str,
    container: &str,
) -> String {
    Api::<Pod>::namespaced(kube_client.clone(), &config().app_ns)
        .logs(
            pod_name,
            &LogParams {
                container: Some(container.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|_| format!("<could not fetch {container} logs>"))
}

/// Creates a fresh single-use pre-auth key in the headscale instance for a
/// test tailscale client. Calls `headscale preauthkeys create` via kubectl exec
/// into the running headscale-0 pod so no separate CRD is required.
async fn create_test_client_auth_key() -> String {
    let output = super::kubectl()
        .args([
            "exec",
            "-n",
            &config().test_ns,
            "headscale-server-main-0",
            "--",
            "headscale",
            "preauthkeys",
            "create",
            "--tags",
            "tag:server",
            "--output",
            "json",
        ])
        .output()
        .await
        .expect("kubectl exec headscale preauthkeys create");

    assert!(
        output.status.success(),
        "headscale preauthkeys create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("headscale preauthkeys create output must be valid JSON");
    json["key"]
        .as_str()
        .expect("JSON response must have 'key' field")
        .to_string()
}

/// Creates a pod named `name` with a tailscale userspace sidecar (SOCKS5 at
/// port 1080) and a curl container. Joins `headscale_server_url` with
/// `auth_key` and waits for the curl container to be Running before returning.
async fn launch_test_client_pod(
    kube_client: &kube::Client,
    name: &str,
    headscale_server_url: &str,
    auth_key: &str,
) {
    let pod_api = Api::<Pod>::namespaced(kube_client.clone(), &config().app_ns);

    let _ = pod_api.delete(name, &DeleteParams::default()).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match pod_api.get(name).await {
            Err(kube::Error::Api(e)) if e.code == 404 => break,
            Ok(_) => {}
            Err(e) => panic!("error waiting for old test pod to delete: {e}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for old test pod to be deleted"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    pod_api
        .create(
            &PostParams::default(),
            &Pod {
                metadata: ObjectMeta {
                    name: Some(name.to_string()),
                    namespace: Some(config().app_ns.clone()),
                    ..Default::default()
                },
                spec: Some(PodSpec {
                    containers: vec![
                        Container {
                            name: "tailscale".to_string(),
                            image: Some("tailscale/tailscale:stable".to_string()),
                            image_pull_policy: Some("IfNotPresent".to_string()),
                            env: Some(vec![
                                EnvVar {
                                    name: "TS_USERSPACE".to_string(),
                                    value: Some("true".to_string()),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_AUTHKEY".to_string(),
                                    value: Some(auth_key.to_string()),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_EXTRA_ARGS".to_string(),
                                    value: Some(format!("--login-server={}", headscale_server_url)),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_SOCKS5_SERVER".to_string(),
                                    value: Some("0.0.0.0:1080".to_string()),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_STATE_DIR".to_string(),
                                    value: Some("/tmp/tailscale-state".to_string()),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_KUBE_SECRET".to_string(),
                                    value: Some(String::new()),
                                    ..Default::default()
                                },
                            ]),
                            volume_mounts: Some(vec![VolumeMount {
                                name: "tailscale-state".to_string(),
                                mount_path: "/tmp/tailscale-state".to_string(),
                                ..Default::default()
                            }]),
                            ..Default::default()
                        },
                        Container {
                            name: "curl".to_string(),
                            image: Some("curlimages/curl:latest".to_string()),
                            image_pull_policy: Some("IfNotPresent".to_string()),
                            command: Some(vec!["sleep".to_string(), "3600".to_string()]),
                            ..Default::default()
                        },
                    ],
                    volumes: Some(vec![Volume {
                        name: "tailscale-state".to_string(),
                        empty_dir: Some(Default::default()),
                        ..Default::default()
                    }]),
                    restart_policy: Some("Never".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("create test-client pod");

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let pod = pod_api.get(name).await.expect("get test-client pod");
        let curl_running = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .into_iter()
            .flatten()
            .any(|cs| {
                cs.name == "curl" && cs.state.as_ref().and_then(|s| s.running.as_ref()).is_some()
            });
        if curl_running {
            break;
        }
        let fatal = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .into_iter()
            .flatten()
            .find_map(|cs| {
                cs.state
                    .as_ref()
                    .and_then(|s| s.waiting.as_ref())
                    .and_then(|w| w.reason.as_deref())
                    .filter(|r| {
                        matches!(*r, "ErrImagePull" | "ImagePullBackOff" | "CrashLoopBackOff")
                    })
                    .map(|r| r.to_string())
            });
        if let Some(reason) = fatal {
            panic!("test-client pod is {reason}");
        }
        // With restart_policy=Never a failed container goes to Terminated rather than
        // CrashLoopBackOff; check for non-zero exit codes and surface logs immediately.
        for cs in pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .into_iter()
            .flatten()
        {
            if let Some(t) = cs.state.as_ref().and_then(|s| s.terminated.as_ref())
                && t.exit_code != 0
            {
                let logs = container_logs_for_pod(kube_client, name, &cs.name).await;
                panic!(
                    "container '{}' in test-client pod '{}' terminated with exit code {}; logs:\n{}",
                    cs.name, name, t.exit_code, logs
                );
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for test-client pod curl container to start"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Marker test: passes iff setup (including `wait_for_ready`) succeeds.
/// If pods crash during bring-up, `wait_for_ready` fails setup and this test
/// surfaces that failure under a descriptive name in the test output.
#[tokio::test]
async fn no_crashed_pods() {
    setup().await;
}

#[tokio::test]
async fn ingress_responds() {
    let state = setup().await;
    for hostname in &state.server_hostnames {
        let http = reqwest::ClientBuilder::new()
            .resolve(hostname, config().ingress_addr)
            .build()
            .unwrap();
        let resp = http
            .get(format!("http://{hostname}/key?v=65"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("HTTP request to {hostname} via ingress failed: {e}"));
        assert_eq!(
            resp.status().as_u16(),
            200,
            "expected 200 from {hostname}/key?v=65"
        );
        let body = resp.text().await.expect("failed to read /key body");
        assert!(
            body.contains("\"publicKey\"") && body.contains("\"mkey:"),
            "/key?v=65 should return JSON with publicKey, got: {body:?}"
        );
    }
}

/// Verifies that the hello-world service is reachable through the tailnet.
///
/// Spins up a Tailscale client pod in userspace mode that joins the same tailnet
/// as the proxy, then uses kubectl exec + curl over the SOCKS5 proxy to reach
/// the hello-world whoami service at its tailnet FQDN.
#[tokio::test]
async fn ingress_hello_world_reachable_via_tailscale() {
    setup().await;

    let kube_client = client().await;

    // Derive the tailnet FQDN from the Ingress + its HeadscaleInstance.
    let ing = Api::<Ingress>::namespaced(kube_client.clone(), &config().app_ns)
        .get("hello-world")
        .await
        .expect("Ingress hello-world must exist after setup");
    let ing_config: serde_json::Value = ing
        .annotations()
        .get("headmaster.potatonode.github.io/config")
        .and_then(|s| serde_json::from_str(s).ok())
        .expect("Ingress hello-world must have a valid config annotation");
    let headscale_ref = ing_config["headscale-ref"]
        .as_str()
        .expect("config must contain headscale-ref")
        .to_string();
    let hostname = ing_config["hostname"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| ing.name_any());
    let dns_base_domain =
        Api::<HeadscaleInstance>::namespaced(kube_client.clone(), &config().test_ns)
            .get(&headscale_ref)
            .await
            .expect("HeadscaleInstance must exist")
            .spec
            .dns_base_domain;
    let tailnet_hostname = format!("{hostname}.{dns_base_domain}");

    let auth_key = create_test_client_auth_key().await;
    let internal_headscale_url = format!(
        "http://headscale-server-{headscale_ref}.{}.svc.cluster.local:8080",
        config().test_ns
    );
    launch_test_client_pod(
        &kube_client,
        "tailscale-curl-client",
        &internal_headscale_url,
        &auth_key,
    )
    .await;

    let target_url = format!("http://{tailnet_hostname}/");
    let pod_api = Api::<Pod>::namespaced(kube_client.clone(), &config().app_ns);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        // Fail fast if tailscale container has terminated before the curl succeeds.
        let pod = pod_api
            .get("tailscale-curl-client")
            .await
            .expect("get tailscale-curl-client pod");
        if let Some(exit_code) = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .into_iter()
            .flatten()
            .find(|cs| cs.name == "tailscale")
            .and_then(|cs| cs.state.as_ref().and_then(|s| s.terminated.as_ref()))
            .filter(|t| t.exit_code != 0)
            .map(|t| t.exit_code)
        {
            let logs =
                container_logs_for_pod(&kube_client, "tailscale-curl-client", "tailscale").await;
            panic!("tailscale container exited with code {exit_code}; logs:\n{logs}");
        }

        let output = super::kubectl()
            .args([
                "exec",
                "-n",
                &config().app_ns,
                "tailscale-curl-client",
                "-c",
                "curl",
                "--",
                "curl",
                "-fv",
                "--max-time",
                "10",
                "--proxy",
                "socks5h://localhost:1080",
                &target_url,
            ])
            .output()
            .await
            .expect("kubectl exec curl");

        if output.status.success() {
            let body = String::from_utf8_lossy(&output.stdout);
            assert!(
                body.contains("Hostname:"),
                "expected whoami response containing 'Hostname:', got: {body:?}"
            );
            break;
        }

        if std::time::Instant::now() >= deadline {
            let ts_logs =
                container_logs_for_pod(&kube_client, "tailscale-curl-client", "tailscale").await;
            panic!(
                "timed out: could not reach hello-world via tailscale after 120s\nurl: {target_url}\nstdout: {}\nstderr: {}\ntailscale logs:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                ts_logs,
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let _ = Api::<Pod>::namespaced(kube_client.clone(), &config().app_ns)
        .delete("tailscale-curl-client", &DeleteParams::default())
        .await;
}

/// Verifies that the proxy node is reachable via a direct (non-DERP) Tailscale
/// connection. Both the proxy pod and the test client pod run inside the same
/// k3d cluster network, so WireGuard can establish a direct path between them.
#[tokio::test]
async fn ingress_direct_connection() {
    setup().await;

    let kube_client = client().await;

    let ing_api = Api::<Ingress>::namespaced(kube_client.clone(), &config().app_ns);
    let ing = ing_api
        .get("hello-world")
        .await
        .expect("Ingress hello-world must exist after setup");
    let headscale_ref = ing
        .annotations()
        .get("headmaster.potatonode.github.io/config")
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v["headscale-ref"].as_str().map(String::from))
        .expect("Ingress hello-world must have headscale-ref in config annotation");
    let internal_headscale_url = format!(
        "http://headscale-server-{headscale_ref}.{}.svc.cluster.local:8080",
        config().test_ns
    );

    // Wait for the proxy to register with headscale and write its tailnet IP
    // into the Ingress loadBalancer status. The operator sets this after the
    // proxy StatefulSet pod starts and containerboot reports device_ips.
    let tailnet_ip = {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            let ing = ing_api
                .get("hello-world")
                .await
                .expect("Ingress hello-world must exist");
            if let Some(ip) = ing
                .status
                .as_ref()
                .and_then(|s| s.load_balancer.as_ref())
                .and_then(|lb| lb.ingress.as_ref())
                .and_then(|i| i.first())
                .and_then(|e| e.ip.as_ref())
                .cloned()
            {
                break ip;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for Ingress hello-world to get a tailnet IP in loadBalancer status"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };

    let auth_key = create_test_client_auth_key().await;
    launch_test_client_pod(
        &kube_client,
        "tailscale-ping-client",
        &internal_headscale_url,
        &auth_key,
    )
    .await;

    // `tailscale ping --until-direct` exits 0 only when a direct (non-DERP) path
    // is established. The daemon may still be in NeedsLogin state right after the
    // pod starts — we retry until the deadline rather than failing immediately on
    // "Logged out." so the race between pod start and authentication is handled.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let output = super::kubectl()
            .args([
                "exec",
                "-n",
                &config().app_ns,
                "tailscale-ping-client",
                "-c",
                "tailscale",
                "--",
                "tailscale",
                "ping",
                "--until-direct",
                "--timeout=30s",
                &tailnet_ip,
            ])
            .output()
            .await
            .expect("kubectl exec tailscale ping");

        if output.status.success() {
            break;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if std::time::Instant::now() >= deadline {
            let ts_logs =
                container_logs_for_pod(&kube_client, "tailscale-ping-client", "tailscale").await;
            if stdout.contains("Logged out.") {
                panic!(
                    "tailscale still not authenticated after 120s — login server may be misconfigured; containerboot logs:\n{ts_logs}"
                );
            }
            panic!(
                "tailscale ping --until-direct must succeed (direct connection required)\nstdout: {stdout}\nstderr: {}\ntailscale logs:\n{ts_logs}",
                String::from_utf8_lossy(&output.stderr),
            );
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let _ = Api::<Pod>::namespaced(kube_client.clone(), &config().app_ns)
        .delete("tailscale-ping-client", &DeleteParams::default())
        .await;
}

/// Verifies that a tags-only Ingress (no `user` annotation, only `managed-key-tags`)
/// results in a proxy StatefulSet that becomes ready.
///
/// This validates the full tags-only path: the operator creates a pre-auth key
/// with user_id=0 and the requested tags, headscale accepts it, and the
/// tailscale proxy pod registers and reaches readyReplicas=1.
#[tokio::test]
async fn ingress_tags_only_proxy_provisions() {
    setup().await;
    let kube_client = client().await;

    let ingress_name = "tags-only-test";
    let ingress_api = Api::<Ingress>::namespaced(kube_client.clone(), &config().app_ns);

    // Clean up any leftover from a previous run.
    let _ = ingress_api
        .delete(ingress_name, &DeleteParams::default())
        .await;

    ingress_api
        .create(
            &PostParams::default(),
            &Ingress {
                metadata: ObjectMeta {
                    name: Some(ingress_name.to_string()),
                    namespace: Some(config().app_ns.clone()),
                    annotations: Some(BTreeMap::from([(
                        "headmaster.potatonode.github.io/config".to_string(),
                        r#"{"headscale-ref":"main","managed-key-tags":["tag:server"]}"#.to_string(),
                    )])),
                    ..Default::default()
                },
                spec: Some(IngressSpec {
                    ingress_class_name: Some("headmaster".to_string()),
                    rules: Some(vec![IngressRule {
                        http: Some(HTTPIngressRuleValue {
                            paths: vec![HTTPIngressPath {
                                path: Some("/".to_string()),
                                path_type: "Prefix".to_string(),
                                backend: IngressBackend {
                                    service: Some(IngressServiceBackend {
                                        name: "hello-world".to_string(),
                                        port: Some(ServiceBackendPort {
                                            number: Some(80),
                                            ..Default::default()
                                        }),
                                    }),
                                    ..Default::default()
                                },
                            }],
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                status: None,
            },
        )
        .await
        .expect("create tags-only Ingress");

    let proxy_name = operator::controllers::ingress::proxy_sts_name(&config().app_ns, ingress_name);
    let sts_api = Api::<StatefulSet>::namespaced(kube_client.clone(), &config().test_ns);

    // Wait for the StatefulSet to be created (proves the operator provisioned the auth key).
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if sts_api.get(&proxy_name).await.is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: proxy StatefulSet {proxy_name} was not created within 60s — \
             the operator may have failed to create a pre-auth key with user_id=0"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Wait for readyReplicas=1 (proves the auth key is accepted for node registration).
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let sts = sts_api
            .get(&proxy_name)
            .await
            .expect("get proxy StatefulSet");
        if sts
            .status
            .as_ref()
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0)
            >= 1
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: proxy StatefulSet {proxy_name} did not reach readyReplicas=1 within 120s"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Clean up: delete the Ingress and wait for the proxy to be removed.
    ingress_api
        .delete(ingress_name, &DeleteParams::default())
        .await
        .expect("delete tags-only Ingress");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match sts_api.get(&proxy_name).await {
            Err(kube::Error::Api(e)) if e.code == 404 => break,
            Ok(_) => {}
            Err(e) => panic!("error waiting for proxy StatefulSet cleanup: {e}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: proxy StatefulSet {proxy_name} was not cleaned up after Ingress deletion"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ── access grant e2e tests ────────────────────────────────────────────────────

/// Creates a headscale user via exec if it doesn't already exist.
async fn ensure_headscale_user(name: &str) {
    let existing = super::headscale_exec(&["headscale", "users", "list", "--output", "json"]).await;
    let users: serde_json::Value =
        serde_json::from_str(existing.trim()).unwrap_or(serde_json::json!([]));
    let already_exists = users
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|u| u["name"].as_str() == Some(name) || u["username"].as_str() == Some(name))
        })
        .unwrap_or(false);
    if !already_exists {
        super::headscale_exec(&["headscale", "users", "create", name]).await;
    }
}

/// Verifies that creating an Ingress with an `access` grant causes the
/// HeadscaleInstance controller to write the corresponding `grants` entry
/// into the live headscale policy.
#[tokio::test]
async fn ingress_access_grant_policy_contains_grant() {
    setup().await;

    let kube_client = client().await;
    let ingress_name = "cap-policy-test";
    let user_name = "cap-policy-user";

    ensure_headscale_user(user_name).await;

    let ingress_api = Api::<Ingress>::namespaced(kube_client.clone(), &config().app_ns);

    // Clean up any leftover from a previous run.
    let _ = ingress_api
        .delete(ingress_name, &DeleteParams::default())
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    ingress_api
        .create(
            &PostParams::default(),
            &Ingress {
                metadata: ObjectMeta {
                    name: Some(ingress_name.to_string()),
                    namespace: Some(config().app_ns.clone()),
                    annotations: Some(BTreeMap::from([(
                        "headmaster.potatonode.github.io/config".to_string(),
                        serde_json::json!({
                            "headscale-ref": "main",
                            "user": user_name,
                            "access": [{
                                "from": ["tag:server"],
                                "capabilities": {
                                    "test.example.com/cap/role": [{"role": "reader"}]
                                }
                            }]
                        })
                        .to_string(),
                    )])),
                    ..Default::default()
                },
                spec: Some(IngressSpec {
                    ingress_class_name: Some("headmaster".to_string()),
                    rules: Some(vec![IngressRule {
                        http: Some(HTTPIngressRuleValue {
                            paths: vec![HTTPIngressPath {
                                path: Some("/".to_string()),
                                path_type: "Prefix".to_string(),
                                backend: IngressBackend {
                                    service: Some(IngressServiceBackend {
                                        name: "hello-world".to_string(),
                                        port: Some(ServiceBackendPort {
                                            number: Some(80),
                                            ..Default::default()
                                        }),
                                    }),
                                    ..Default::default()
                                },
                            }],
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                status: None,
            },
        )
        .await
        .expect("create cap-policy-test Ingress");

    let expected_auto_tag =
        operator::controllers::ingress::ingress_auto_tag(&config().app_ns, ingress_name);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let policy = super::headscale_policy().await;

        let has_grant = policy["grants"].as_array().is_some_and(|grants| {
            grants.iter().any(|g| {
                g["dst"]
                    .as_array()
                    .is_some_and(|dst| dst.iter().any(|d| d.as_str() == Some(&expected_auto_tag)))
            })
        });

        if has_grant {
            // Validate the grant content.
            let grant = policy["grants"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| {
                    g["dst"].as_array().is_some_and(|dst| {
                        dst.iter().any(|d| d.as_str() == Some(&expected_auto_tag))
                    })
                })
                .expect("grant must exist");
            assert_eq!(
                grant["src"][0], "tag:server",
                "grant source must be 'tag:server'"
            );
            assert!(
                !grant["app"]["test.example.com/cap/role"].is_null(),
                "grant must contain the capability"
            );
            break;
        }

        assert!(
            std::time::Instant::now() < deadline,
            "timed out: headscale policy did not contain the expected grant after 60s\n\
             expected tag: {expected_auto_tag}\npolicy: {policy}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Clean up.
    let _ = ingress_api
        .delete(ingress_name, &DeleteParams::default())
        .await;
}

// NOTE: The Ingress admission webhook (`validate_cap_name`) is tested at the
// unit level in `operator/src/server/webhook.rs`. An e2e test is omitted here
// because the examples chart deploys the operator with `webhook.enabled: false`
// (cert-manager is not available in the k3d test cluster).

/// Verifies end-to-end that the `Tailscale-App-Capabilities` header is forwarded
/// to the upstream application when an Ingress has a matching access grant with
/// capabilities. Uses the existing tailscale test client infrastructure.
#[tokio::test]
async fn ingress_capability_header_forwarded() {
    setup().await;

    let kube_client = client().await;
    let ingress_name = "cap-header-test";
    let user_name = "cap-header-user";

    ensure_headscale_user(user_name).await;

    let ingress_api = Api::<Ingress>::namespaced(kube_client.clone(), &config().app_ns);
    let _ = ingress_api
        .delete(ingress_name, &DeleteParams::default())
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Read the headscale-ref from the existing hello-world Ingress so we
    // can find the correct dns_base_domain.
    let hi_api = Api::<HeadscaleInstance>::namespaced(kube_client.clone(), &config().test_ns);
    let dns_base_domain = hi_api
        .get("main")
        .await
        .expect("HeadscaleInstance 'main' must exist")
        .spec
        .dns_base_domain;
    let tailnet_fqdn = format!("{ingress_name}.{dns_base_domain}");

    ingress_api
        .create(
            &PostParams::default(),
            &Ingress {
                metadata: ObjectMeta {
                    name: Some(ingress_name.to_string()),
                    namespace: Some(config().app_ns.clone()),
                    annotations: Some(BTreeMap::from([(
                        "headmaster.potatonode.github.io/config".to_string(),
                        serde_json::json!({
                            "headscale-ref": "main",
                            "user": user_name,
                            "access": [{
                                "from": ["tag:server"],
                                "capabilities": {
                                    "test.example.com/cap/role": [{"role": "e2e-reader"}]
                                }
                            }]
                        })
                        .to_string(),
                    )])),
                    ..Default::default()
                },
                spec: Some(IngressSpec {
                    ingress_class_name: Some("headmaster".to_string()),
                    rules: Some(vec![IngressRule {
                        http: Some(HTTPIngressRuleValue {
                            paths: vec![HTTPIngressPath {
                                path: Some("/".to_string()),
                                path_type: "Prefix".to_string(),
                                backend: IngressBackend {
                                    service: Some(IngressServiceBackend {
                                        name: "hello-world".to_string(),
                                        port: Some(ServiceBackendPort {
                                            number: Some(80),
                                            ..Default::default()
                                        }),
                                    }),
                                    ..Default::default()
                                },
                            }],
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                status: None,
            },
        )
        .await
        .expect("create cap-header-test Ingress");

    let expected_auto_tag =
        operator::controllers::ingress::ingress_auto_tag(&config().app_ns, ingress_name);

    // Wait for the policy to contain the grant.
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let policy = super::headscale_policy().await;
            let has_grant = policy["grants"].as_array().is_some_and(|grants| {
                grants.iter().any(|g| {
                    g["dst"].as_array().is_some_and(|dst| {
                        dst.iter().any(|d| d.as_str() == Some(&expected_auto_tag))
                    })
                })
            });
            if has_grant {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out: headscale policy did not contain grant for {expected_auto_tag}"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    // Wait for the proxy to register (Ingress gets a tailnet IP).
    let internal_headscale_url = format!(
        "http://headscale-server-main.{}.svc.cluster.local:8080",
        config().test_ns
    );
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            let ing = ingress_api
                .get(ingress_name)
                .await
                .expect("get cap-header-test Ingress");
            let has_ip = ing
                .status
                .as_ref()
                .and_then(|s| s.load_balancer.as_ref())
                .and_then(|lb| lb.ingress.as_ref())
                .is_some_and(|i| !i.is_empty());
            if has_ip {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out: cap-header-test Ingress did not get a tailnet IP"
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    // Launch test client and curl through the proxy. The source node has tag:server
    // which matches the grant's "from" field; the proxy should forward the capability header.
    let auth_key = create_test_client_auth_key().await;
    launch_test_client_pod(
        &kube_client,
        "cap-header-client",
        &internal_headscale_url,
        &auth_key,
    )
    .await;

    let target_url = format!("http://{tailnet_fqdn}/");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let response_body = loop {
        let output = super::kubectl()
            .args([
                "exec",
                "-n",
                &config().app_ns,
                "cap-header-client",
                "-c",
                "curl",
                "--",
                "curl",
                "-fs",
                "--max-time",
                "10",
                "--proxy",
                "socks5h://localhost:1080",
                &target_url,
            ])
            .output()
            .await
            .expect("kubectl exec curl");

        if output.status.success() {
            break String::from_utf8_lossy(&output.stdout).into_owned();
        }

        if std::time::Instant::now() >= deadline {
            let ts_logs =
                container_logs_for_pod(&kube_client, "cap-header-client", "tailscale").await;
            panic!(
                "timed out: could not reach {target_url} via tailscale\n\
                 stderr: {}\ntailscale logs:\n{ts_logs}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };

    assert!(
        response_body
            .to_lowercase()
            .contains("tailscale-app-capabilities"),
        "response from whoami must contain the Tailscale-App-Capabilities header; \
         response body:\n{response_body}"
    );

    // Clean up.
    let _ = Api::<Pod>::namespaced(kube_client.clone(), &config().app_ns)
        .delete("cap-header-client", &DeleteParams::default())
        .await;
    let _ = ingress_api
        .delete(ingress_name, &DeleteParams::default())
        .await;
}
