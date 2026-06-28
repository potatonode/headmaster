use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;
use crate::types::ScimError;

pub async fn require_bearer(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ScimError> {
    let token = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(ScimError::unauthorized)?;

    // Not constant-time — acceptable for an internal SCIM endpoint protected
    // by a static bearer token on a private network.
    if token == state.scim_token.as_str() {
        Ok(next.run(request).await)
    } else {
        Err(ScimError::unauthorized())
    }
}
