//! E2e test binary — one k3d cluster shared across all test modules.
//!
//! Run with: `task test-e2e`
//! Requires: `task deploy-operator` to have succeeded first.
//! `KUBE_CONTEXT` selects which kubeconfig context to use (defaults to the current context).
//!
//! # Why `#[path]` on the module declarations below
//!
//! This file is the test binary crate root at `tests/e2e.rs`, so a bare
//! `mod headscale_instance;` would resolve to `tests/headscale_instance.rs` (a
//! sibling), not `tests/e2e/headscale_instance.rs`. The `#[path]` attribute
//! overrides that lookup so all test modules compile into a single binary.

// `#[path]` needed: see module-resolution note in the file header above.
#[path = "e2e/examples.rs"]
mod examples;

#[path = "e2e/pocket_id.rs"]
mod pocket_id;

use std::net::SocketAddr;
use std::sync::OnceLock;

use rustls::crypto::aws_lc_rs as aws_lc_rs_provider;

use kube::Client;

// ── shared helpers ────────────────────────────────────────────────────────────

pub struct E2eConfig {
    /// Address of the k3d ingress (e.g. `127.0.0.1:8080`). Set via `INGRESS_ADDR`.
    pub ingress_addr: SocketAddr,
    /// Kubernetes context to use. Set via `KUBE_CONTEXT`.
    pub kube_context: String,
    /// Kubernetes namespace where the examples are installed. Set via `TEST_NS`.
    pub test_ns: String,
    /// Kubernetes namespace where the demo app (hello-world) lives. Set via `APP_NS`.
    pub app_ns: String,
    /// Base URL of the headscale server ingress. Set via `HEADSCALE_SERVER_URL`.
    pub headscale_server_url: String,
    /// Hostname of the Pocket ID ingress. Set via `POCKET_ID_HOSTNAME`.
    pub pocket_id_hostname: String,
    /// Pocket ID static API key. Set via `POCKET_ID_API_KEY`.
    pub pocket_id_api_key: String,
    /// OIDC client ID declared in the examples chart. Set via `OIDC_CLIENT_ID`.
    pub oidc_client_id: String,
}

/// Returns the e2e configuration, parsed from environment variables once and
/// cached for the lifetime of the process.
pub fn config() -> &'static E2eConfig {
    static CONFIG: OnceLock<E2eConfig> = OnceLock::new();
    // envtest uses kube with default-features, pulling in ring. Both
    // ring and aws-lc-rs are compiled into the test binary; install
    // aws-lc-rs as the default to break the tie.
    let _ = aws_lc_rs_provider::default_provider().install_default();
    CONFIG.get_or_init(|| E2eConfig {
        ingress_addr: std::env::var("INGRESS_ADDR")
            .expect("INGRESS_ADDR must be set")
            .parse()
            .expect("INGRESS_ADDR must be a valid socket address"),
        kube_context: std::env::var("KUBE_CONTEXT").expect("KUBE_CONTEXT must be set"),
        test_ns: std::env::var("TEST_NS").expect("TEST_NS must be set"),
        app_ns: std::env::var("APP_NS").expect("APP_NS must be set"),
        headscale_server_url: std::env::var("HEADSCALE_SERVER_URL")
            .expect("HEADSCALE_SERVER_URL must be set"),
        pocket_id_hostname: std::env::var("POCKET_ID_HOSTNAME")
            .expect("POCKET_ID_HOSTNAME must be set"),
        pocket_id_api_key: std::env::var("POCKET_ID_API_KEY")
            .expect("POCKET_ID_API_KEY must be set"),
        oidc_client_id: std::env::var("OIDC_CLIENT_ID").expect("OIDC_CLIENT_ID must be set"),
    })
}

/// Returns a [`Client`] for the context named by `KUBE_CONTEXT`.
pub async fn client() -> Client {
    integration_tests::kube_client()
        .await
        .expect("kube client (is KUBE_CONTEXT set to the k3d cluster context?)")
}

/// Returns a [`tokio::process::Command`] for `kubectl` with `--context` pre-set.
pub fn kubectl() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("kubectl");
    cmd.args(["--context", &config().kube_context]);
    cmd
}

/// Runs a command inside the `headscale` container of `headscale-server-main-0`
/// and returns its stdout as a `String`.
pub async fn headscale_exec(cmd: &[&str]) -> String {
    use k8s_openapi::api::core::v1::Pod;
    use kube::Api;
    use kube::api::AttachParams;
    use tokio::io::AsyncReadExt;

    let kube = client().await;
    let pods: Api<Pod> = Api::namespaced(kube, &config().test_ns);
    let mut process = pods
        .exec(
            "headscale-server-main-0",
            cmd.iter().copied(),
            &AttachParams::default()
                .container("headscale")
                .stdout(true)
                .stderr(true)
                .stdin(false),
        )
        .await
        .unwrap_or_else(|e| panic!("headscale_exec {cmd:?}: {e}"));
    let mut stdout = process.stdout().expect("exec produced no stdout handle");
    let mut stderr = process.stderr().expect("exec produced no stderr handle");
    let (stdout_out, stderr_out) = tokio::join!(
        async {
            let mut s = String::new();
            stdout
                .read_to_string(&mut s)
                .await
                .expect("read exec stdout");
            s
        },
        async {
            let mut s = String::new();
            stderr
                .read_to_string(&mut s)
                .await
                .expect("read exec stderr");
            s
        },
    );
    process.join().await.unwrap_or_else(|e| {
        panic!("headscale_exec {cmd:?} exited with error: {e}\nstderr: {stderr_out}")
    });
    stdout_out
}

/// Returns the live headscale policy parsed as JSON.
pub async fn headscale_policy() -> serde_json::Value {
    let output = headscale_exec(&["headscale", "policy", "get"]).await;
    jsonc_parser::parse_to_serde_value::<serde_json::Value>(
        output.trim(),
        &jsonc_parser::ParseOptions::default(),
    )
    .unwrap_or_else(|e| panic!("headscale policy get output must be JSONC: {e}\n{output}"))
}
