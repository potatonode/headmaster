//! E2e tests for the Pocket ID → headmaster-scim → headscale provisioning flow.
//!
//! Setup (once per test binary): waits for all namespace resources to be ready,
//! creates the groups each test needs, and allows those groups on the headmaster
//! OIDC client so that users in them are included in SCIM syncs.
//!
//! Tests create a user, add it to its dedicated group, trigger an immediate SCIM
//! sync, then assert the result via the headscale CLI inside the pod.

use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{ApiResource, AttachParams, DynamicObject};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;

use super::{client, config};

// ── Pocket ID client ──────────────────────────────────────────────────────────

struct PocketIdClient {
    /// reqwest client with `X-API-Key` baked into default headers.
    http: reqwest::Client,
    base: String,
    scim_provider_id: String,
    /// Group pre-created for `scim_user_propagates_to_headscale`.
    alice_group_id: String,
    /// Group pre-created for `scim_group_propagates_to_headscale_policy`.
    bob_group_id: String,
}

impl PocketIdClient {
    async fn create_user(
        &self,
        username: &str,
        first_name: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let resp: Value = self
            .http
            .post(format!("{}/api/users", self.base))
            .json(&json!({
                "username": username,
                "firstName": first_name,
                "lastName": "ScimTest",
                "email": format!("{username}@example.com"),
            }))
            .send()
            .await?
            .json()
            .await?;
        resp["id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| format!("create user response missing id: {resp}").into())
    }

    async fn set_group_members(
        &self,
        group_id: &str,
        user_ids: &[&str],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let r = self
            .http
            .put(format!("{}/api/user-groups/{group_id}/users", self.base))
            .json(&json!({ "userIds": user_ids }))
            .send()
            .await?;
        if !r.status().is_success() {
            let body = r.text().await.unwrap_or_default();
            return Err(format!("set group members failed: {body}").into());
        }
        Ok(())
    }

    async fn trigger_scim_sync(&self) -> Result<(), Box<dyn std::error::Error>> {
        let id = &self.scim_provider_id;
        let r = self
            .http
            .post(format!("{}/api/scim/service-provider/{id}/sync", self.base))
            .send()
            .await?;
        if !r.status().is_success() {
            let body = r.text().await.unwrap_or_default();
            return Err(format!("SCIM sync trigger failed: {body}").into());
        }
        Ok(())
    }
}

// ── setup ─────────────────────────────────────────────────────────────────────

static SETUP: OnceCell<Result<Arc<PocketIdClient>, String>> = OnceCell::const_new();

async fn setup() -> Arc<PocketIdClient> {
    SETUP
        .get_or_init(|| async { do_setup().await.map_err(|e| e.to_string()) })
        .await
        .as_ref()
        .unwrap_or_else(|e| panic!("pocket-id e2e setup failed: {e}"))
        .clone()
}

async fn do_setup() -> Result<Arc<PocketIdClient>, Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let cfg = config();
    let kube = client().await;
    integration_tests::wait_for_namespace_ready(&kube, &cfg.test_ns).await?;

    let hostname = cfg.pocket_id_hostname.as_str();
    let base = format!("http://{hostname}");

    let mut default_headers = HeaderMap::new();
    default_headers.insert(
        "x-api-key",
        HeaderValue::from_str(&cfg.pocket_id_api_key).expect("API key is valid header value"),
    );
    let http = reqwest::ClientBuilder::new()
        .resolve(hostname, cfg.ingress_addr)
        .timeout(Duration::from_secs(10))
        .default_headers(default_headers)
        .build()?;

    // Verify the pocket-id operator registered the SCIM provider. scimProviderID
    // is absent when tokenSecretRef.key doesn't match the actual secret field
    // (e.g. "token" vs "SCIM_BEARER_TOKEN"), causing a silent partial success.
    // The operator writes scimProviderID asynchronously after reconciling the
    // PocketIDOIDCClient, so retry until it appears rather than checking once.
    let oidc_ar = ApiResource {
        group: "pocketid.internal".into(),
        version: "v1alpha1".into(),
        api_version: "pocketid.internal/v1alpha1".into(),
        kind: "PocketIDOIDCClient".into(),
        plural: "pocketidoidcclients".into(),
    };
    {
        let oidc_api: Api<DynamicObject> =
            Api::namespaced_with(kube.clone(), &cfg.test_ns, &oidc_ar);
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let oidc_client = oidc_api.get(&cfg.oidc_client_id).await?;
            if oidc_client.data["status"]["scimProviderID"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "PocketIDOIDCClient '{}' has no scimProviderID after 120s — \
                     SCIM registration failed; check that tokenSecretRef.key \
                     matches the secret field name",
                    cfg.oidc_client_id
                )
                .into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    // The pocket-id operator registers the SCIM provider asynchronously after
    // reading the headscale-scim-token secret. Helm --wait only guarantees the
    // PocketIDOIDCClient is Ready, not that the registration is visible via the
    // pocket-id REST API yet. Retry until it appears.
    let scim_provider_id = {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let resp = http
                .get(format!(
                    "{base}/api/oidc/clients/{}/scim-service-provider",
                    cfg.oidc_client_id
                ))
                .send()
                .await?;
            let status = resp.status();
            if status.is_success() {
                let body: Value = resp.json().await?;
                break body["id"]
                    .as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| format!("SCIM service provider response missing id: {body}"))?;
            }
            if Instant::now() >= deadline {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!(
                    "SCIM service provider not found after 120s ({status}): {body}"
                )
                .into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };

    // Create the group each test needs. These must exist before the tests run
    // in parallel so that `allowed-user-groups` can be set once atomically.
    let alice_group_id = create_group(&http, &base, "scim-alice-group", "SCIM Alice Group").await?;
    let bob_group_id = create_group(&http, &base, "scim-bob-group", "SCIM Bob Group").await?;

    // Allow both groups on the OIDC client in a single PUT so individual tests
    // never need to mutate the allowed-groups list concurrently.
    let r = http
        .put(format!(
            "{base}/api/oidc/clients/{}/allowed-user-groups",
            cfg.oidc_client_id
        ))
        .json(&json!({ "userGroupIds": [&alice_group_id, &bob_group_id] }))
        .send()
        .await?;
    if !r.status().is_success() {
        let body = r.text().await.unwrap_or_default();
        return Err(format!("allow groups on OIDC client failed: {body}").into());
    }

    Ok(Arc::new(PocketIdClient {
        http,
        base,
        scim_provider_id,
        alice_group_id,
        bob_group_id,
    }))
}

async fn create_group(
    http: &reqwest::Client,
    base: &str,
    name: &str,
    friendly_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let resp: Value = http
        .post(format!("{base}/api/user-groups"))
        .json(&json!({ "name": name, "friendlyName": friendly_name }))
        .send()
        .await?
        .json()
        .await?;
    resp["id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("create group '{name}' response missing id: {resp}").into())
}

// ── headscale query helpers ───────────────────────────────────────────────────

// ── tailnet connectivity helpers ──────────────────────────────────────────────

async fn headscale_exec(cmd: &[&str]) -> String {
    let kube = client().await;
    let pods: Api<Pod> = Api::namespaced(kube, &config().test_ns);
    let mut process = pods
        .exec(
            "headscale-server-main-0",
            cmd.iter().copied(),
            &AttachParams::default()
                .container("headscale")
                .stdout(true)
                .stderr(false)
                .stdin(false),
        )
        .await
        .unwrap_or_else(|e| panic!("exec {cmd:?}: {e}"));
    let mut stdout = process.stdout().expect("exec produced no stdout handle");
    let mut output = String::new();
    stdout
        .read_to_string(&mut output)
        .await
        .expect("read exec stdout");
    drop(stdout);
    process
        .join()
        .await
        .unwrap_or_else(|e| panic!("exec {cmd:?} exited with error: {e}"));
    output
}

async fn headscale_users() -> Value {
    let output = headscale_exec(&["headscale", "users", "list", "--output", "json"]).await;
    serde_json::from_str(output.trim()).expect("headscale users list output must be JSON")
}

async fn headscale_policy() -> Value {
    let output = headscale_exec(&["headscale", "policy", "get"]).await;
    // headscale stores and returns the policy as HuJSON, preserving block comments.
    // Use a JSONC-aware parser so ExternalId-mode block comments don't break parsing.
    jsonc_parser::parse_to_serde_value::<Value>(
        output.trim(),
        &jsonc_parser::ParseOptions::default(),
    )
    .expect("headscale policy get output must be parseable as JSONC")
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Creates a user in Pocket ID, syncs via SCIM, and asserts that:
/// - The user's email appears in the headscale ACL policy (proving SCIM
///   stored the user and resolved their email for group membership).
/// - The user does NOT appear in headscale users (OIDC owns user creation;
///   headscale creates the user on first OIDC login, not via SCIM).
#[tokio::test]
async fn scim_user_syncs_to_policy_without_headscale_user() {
    let pocket_id = setup().await;

    let user_id = pocket_id
        .create_user("scim-alice", "Alice")
        .await
        .expect("create Pocket ID user scim-alice");

    pocket_id
        .set_group_members(&pocket_id.alice_group_id, &[&user_id])
        .await
        .expect("add scim-alice to alice group");

    // Initial sync attempt may fail if a concurrent sync is already in progress.
    let _ = pocket_id.trigger_scim_sync().await;

    // In ExternalId mode the policy token is "http://{issuer}/{uuid}@" where
    // uuid is the Pocket ID user UUID (= SCIM externalId = OIDC sub).
    let cfg = config();
    let expected_token = format!("http://{}/{user_id}@", cfg.pocket_id_hostname);
    let group_key = "group:SCIM Alice Group";
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let policy = headscale_policy().await;
        let members = policy
            .get("groups")
            .and_then(|g| g.get(group_key))
            .and_then(|m| m.as_array());

        if let Some(members) = members
            && members
                .iter()
                .any(|m| m.as_str() == Some(expected_token.as_str()))
        {
            break;
        }

        assert!(
            Instant::now() < deadline,
            "timed out: '{group_key}' with token '{expected_token}' did not appear \
             in headscale ACL policy after SCIM sync; policy: {policy}"
        );
        let _ = pocket_id.trigger_scim_sync().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // SCIM must not have created a headscale user — OIDC owns user creation.
    let users = headscale_users().await;
    let alice_in_headscale = users
        .as_array()
        .is_some_and(|arr| arr.iter().any(|u| u["name"] == "scim-alice"));
    assert!(
        !alice_in_headscale,
        "SCIM must not create headscale users; 'scim-alice' appeared in headscale users: {users}"
    );
}

/// Creates a group with a member in Pocket ID, syncs via SCIM, and asserts
/// both the group and its member appear in the headscale ACL policy.
#[tokio::test]
async fn scim_group_propagates_to_headscale_policy() {
    let pocket_id = setup().await;

    let user_id = pocket_id
        .create_user("scim-bob", "Bob")
        .await
        .expect("create Pocket ID user scim-bob");

    pocket_id
        .set_group_members(&pocket_id.bob_group_id, &[&user_id])
        .await
        .expect("add scim-bob to bob group");

    // Initial sync attempt may fail if a concurrent sync is already in progress.
    let _ = pocket_id.trigger_scim_sync().await;

    // Pocket ID sends the group's friendlyName as SCIM displayName, which
    // becomes the headscale policy group key.
    let cfg = config();
    let expected_token = format!("http://{}/{user_id}@", cfg.pocket_id_hostname);
    let group_key = "group:SCIM Bob Group";
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let policy = headscale_policy().await;
        let members = policy
            .get("groups")
            .and_then(|g| g.get(group_key))
            .and_then(|m| m.as_array());

        if let Some(members) = members
            && members
                .iter()
                .any(|m| m.as_str() == Some(expected_token.as_str()))
        {
            break;
        }

        assert!(
            Instant::now() < deadline,
            "timed out: '{group_key}' with token '{expected_token}' did not appear \
             in headscale ACL policy after SCIM sync; policy: {policy}"
        );
        let _ = pocket_id.trigger_scim_sync().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
