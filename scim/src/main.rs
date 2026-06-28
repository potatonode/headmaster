mod auth;
mod policy;
mod routes;
mod services;
mod storage;
mod types;

use std::path::PathBuf;

use axum::Router;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::get;

use headscale_client::Channel;
use headscale_client::{AuthInterceptor, HeadscaleServiceClient};

use services::{PolicyUserKey, ScimConfig, ScimService};

use shadow_rs::shadow;
shadow!(build);

#[derive(Clone)]
pub struct AppState {
    pub scim: ScimService,
    pub scim_token: String,
}

struct Config {
    headscale_url: String,
    headscale_api_key: String,
    scim_token: String,
    external_id_file: PathBuf,
    listen_addr: String,
    scim_config: ScimConfig,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        fn require(key: &str) -> Result<String, String> {
            std::env::var(key).map_err(|_| format!("missing required env var {key}"))
        }

        let policy_user_key_str =
            std::env::var("POLICY_USER_KEY").unwrap_or_else(|_| "email".to_string());
        let policy_user_key = match policy_user_key_str.as_str() {
            "email" => PolicyUserKey::Email,
            "username" => PolicyUserKey::Username,
            "external_id" => {
                let oidc_issuer = require("OIDC_ISSUER")?.trim_end_matches('/').to_string();
                PolicyUserKey::ExternalId { oidc_issuer }
            }
            other => {
                return Err(format!(
                    "invalid POLICY_USER_KEY '{other}'; expected 'email', 'username', or 'external_id'"
                ));
            }
        };

        let expire_nodes_on_change = std::env::var("EXPIRE_NODES_ON_CHANGE")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .map_err(|_| "invalid EXPIRE_NODES_ON_CHANGE; expected 'true' or 'false'")?;

        Ok(Self {
            headscale_url: require("HEADSCALE_URL")?,
            headscale_api_key: require("HEADSCALE_API_KEY")?,
            scim_token: require("SCIM_BEARER_TOKEN")?,
            external_id_file: PathBuf::from(
                std::env::var("SCIM_EXTERNAL_ID_FILE")
                    .unwrap_or_else(|_| "/data/external-id-map.json".to_string()),
            ),
            listen_addr: std::env::var("SCIM_LISTEN_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8081".to_string()),
            scim_config: ScimConfig {
                policy_user_key,
                expire_nodes_on_change,
            },
        })
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tracing::info!(
        commit = build::SHORT_COMMIT,
        branch = build::BRANCH,
        dirty = !build::GIT_CLEAN,
        built_at = build::BUILD_TIME_3339,
        rustc = build::RUST_VERSION,
        "starting"
    );
    if !build::GIT_CLEAN {
        tracing::warn!(
            files = build::GIT_STATUS_FILE,
            "built from dirty working tree"
        );
    }

    let config = Config::from_env().unwrap_or_else(|e| {
        tracing::error!("{e}");
        std::process::exit(1);
    });

    let channel = Channel::from_shared(config.headscale_url.clone())
        .expect("HEADSCALE_URL must be a valid URI")
        .connect_timeout(std::time::Duration::from_secs(10))
        .connect_lazy();

    let client = HeadscaleServiceClient::with_interceptor(
        channel,
        AuthInterceptor::bearer(&config.headscale_api_key),
    );

    let mapping = storage::load_shared(&config.external_id_file)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("failed to load external ID mapping: {e}");
            std::process::exit(1);
        });

    let state = AppState {
        scim: ScimService::new(client, mapping, config.scim_config),
        scim_token: config.scim_token,
    };

    let protected = Router::new()
        .route(
            "/Users",
            get(routes::users::list_users).post(routes::users::create_user),
        )
        .route(
            "/Users/{id}",
            get(routes::users::get_user)
                .put(routes::users::put_user)
                .delete(routes::users::delete_user),
        )
        .route(
            "/Groups",
            get(routes::groups::list_groups).post(routes::groups::create_group),
        )
        .route(
            "/Groups/{id}",
            get(routes::groups::get_group)
                .put(routes::groups::put_group)
                .delete(routes::groups::delete_group),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    let scim_routes = Router::new()
        .route(
            "/ServiceProviderConfig",
            get(routes::discovery::service_provider_config),
        )
        .route("/Schemas", get(routes::discovery::schemas))
        .route("/ResourceTypes", get(routes::discovery::resource_types))
        .merge(protected)
        .with_state(state);

    let app = Router::new()
        .route("/livez", get(|| async { StatusCode::OK }))
        .route("/readyz", get(|| async { StatusCode::OK }))
        .nest("/scim/v2", scim_routes);

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("failed to bind {}: {e}", config.listen_addr);
            std::process::exit(1);
        });

    tracing::info!("headmaster-scim listening on {}", config.listen_addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| tracing::error!("server error: {e}"));
}

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
