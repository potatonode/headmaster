pub mod discovery;
pub mod groups;
pub mod users;

use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::types::ScimError;

pub(crate) fn scim_json<T: serde::Serialize>(
    status: StatusCode,
    body: &T,
) -> Result<axum::response::Response, ScimError> {
    Ok((
        status,
        [("Content-Type", "application/scim+json")],
        serde_json::to_string(body).map_err(|e| ScimError::internal(e.to_string()))?,
    )
        .into_response())
}
