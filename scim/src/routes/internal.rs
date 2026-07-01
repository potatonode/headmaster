use axum::extract::State;
use axum::http::StatusCode;

use crate::AppState;
use crate::types::ScimError;

pub async fn reconcile(State(state): State<AppState>) -> Result<StatusCode, ScimError> {
    state.scim.reconcile_groups_policy().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    use headscale_client::fake::{FakeHeadscaleServer, spawn_fake_channel};
    use headscale_client::{AuthInterceptor, HeadscaleServiceClient};

    use crate::services::{ScimConfig, ScimService};
    use crate::storage;

    async fn make_state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let mapping = storage::Mapping::load(&dir.path().join("mapping.json"))
            .await
            .unwrap();
        let mapping = storage::shared(mapping);
        let server = FakeHeadscaleServer::default();
        let channel = spawn_fake_channel(server).await;
        let client =
            HeadscaleServiceClient::with_interceptor(channel, AuthInterceptor::bearer("test"));
        let service = ScimService::new(client, mapping, ScimConfig::default());
        AppState {
            scim: service,
            scim_token: "test-token".to_string(),
        }
    }

    #[tokio::test]
    async fn reconcile_returns_no_content() {
        let state = make_state().await;
        let result = reconcile(State(state)).await;
        assert_eq!(result.unwrap(), StatusCode::NO_CONTENT);
    }
}
