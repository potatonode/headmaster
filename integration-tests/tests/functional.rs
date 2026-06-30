//! Functional test binary — one envtest control plane shared across all test modules.
//!
//! Run with: `task test-functional`
//! Requires: Go 1.25+ and libclang (for envtest's build-time FFI generation).
//! On first run, envtest downloads kube-apiserver and etcd binaries and caches them locally.
//!
//! # Why `#[path]` on the module declarations below
//!
//! This file is the test binary crate root at `tests/functional.rs`, so a bare
//! `mod crds;` would resolve to `tests/crds.rs` (a sibling), not
//! `tests/functional/crds.rs`. The `#[path]` attribute overrides that lookup so
//! all test modules compile into a single binary and share one envtest startup.

// `#[path]` needed: see module-resolution note in the file header above.
#[path = "functional/crds.rs"]
mod crds;
#[path = "functional/headscale_instance.rs"]
mod headscale_instance;
#[path = "functional/ingress.rs"]
mod ingress;
#[path = "functional/support.rs"]
pub mod support;

use envtest::{BinaryAssetsSettings, Environment};
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Client;
use kube::api::{Api, PostParams};
use rustls::crypto::aws_lc_rs as aws_lc_rs_provider;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

/// Calls `f` in a loop, retrying on HTTP 429 (storage re-initializing) with a
/// 200 ms back-off.  Any other error is returned immediately.
pub async fn retry_on_429<F, Fut, T>(mut f: F) -> Result<T, kube::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, kube::Error>>,
{
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(kube::Error::Api(ref ae)) if ae.code == 429 => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

// ---- shared environment ----------------------------------------------------

// One kube-apiserver + etcd instance for the entire test binary.
//
// tokio::sync::OnceCell lets the init closure be async, so the test's own
// runtime drives the initialization — no throwaway runtime or extra thread.
// Concurrent tests that call server() simultaneously wait on the OnceCell
// until the first one finishes, then all share the same reference.
//
// Rust does not call Drop on statics at exit, so the #[dtor] below explicitly
// calls destroy() to shut down the envtest processes rather than orphaning them.
//
// Primary thing to note is that while the initialization logic requires async, the
// envtest::Server is just a wrapper around a kubeconfig String. So it should be
// safe to destroy this in a different async runtime.
static SERVER: tokio::sync::OnceCell<envtest::Server> = tokio::sync::OnceCell::const_new();

async fn server() -> &'static envtest::Server {
    SERVER
        .get_or_init(|| async {
            // envtest uses kube with default-features, pulling in ring. Both
            // ring and aws-lc-rs are compiled into the test binary; install
            // aws-lc-rs as the default to break the tie.
            let _ = aws_lc_rs_provider::default_provider().install_default();
            let _ = tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .try_init();
            let mut env = Environment {
                binary_assets_settings: BinaryAssetsSettings {
                    download_binary_assets: true,
                    download_binary_assets_version: Some("1.36.0".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            };

            for crd in operator::crds() {
                env = env.with_crds(crd).expect("CRD serialization failed");
            }
            let server = env.create().await.expect("envtest failed to start");
            // The API server can return 429 "storage is (re)initializing" for a
            // short window after startup. Probe our own CRD kind specifically:
            // built-in types become ready earlier, so probing CRDs alone is not
            // sufficient to confirm that custom-resource storage is ready.
            let probe: Api<operator::types::HeadscaleInstance> =
                Api::all(build_client(&server).await);
            retry_on_429(|| async { probe.list(&Default::default()).await.map(|_| ()) })
                .await
                .expect("envtest CRD storage ready check");
            server
        })
        .await
}

// Runs after main() returns, i.e. after all tests finish. Rust does not call
// Drop on statics at exit, so without this the envtest processes would be
// orphaned on every test run. atexit handlers run on normal exit (including
// process::exit(1) from test failures) but not on abort; that is acceptable
// because abort exits immediately and the OS reclaims child processes.
#[dtor::dtor(unsafe)]
fn destroy_server() {
    if let Some(server) = SERVER.get() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let _ = server.destroy().await;
        });
    }
}

/// Returns a fresh [`Client`] connected to the shared envtest API server.
///
/// Call once at the top of each test; the client's connection pool is tied to
/// that test's tokio runtime and is cleaned up when the test finishes.
pub async fn client() -> Client {
    build_client(server().await).await
}

async fn build_client(server: &envtest::Server) -> Client {
    let kubeconfig = kube::config::Kubeconfig::from_yaml(server.as_ref())
        .expect("failed to parse envtest kubeconfig");
    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &Default::default())
        .await
        .expect("failed to build kube config");
    Client::try_from(config).expect("failed to build kube client")
}

// ---- namespace helpers -----------------------------------------------------

/// Creates a namespace with a unique name and returns it.
///
/// Call at the start of each test to get an isolated namespace so concurrently
/// running tests cannot observe each other's resources.
pub async fn unique_ns(client: &Client) -> String {
    let name = format!("test-{}", &Uuid::new_v4().to_string()[..8]);
    let api: Api<Namespace> = Api::all(client.clone());
    api.create(
        &PostParams::default(),
        &Namespace {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .expect("failed to create test namespace");
    name
}
