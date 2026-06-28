use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::AppState;
use crate::routes::scim_json;
use crate::services::GroupBody;
use crate::types::{ScimError, ScimMember};

pub async fn list_groups(
    State(state): State<AppState>,
) -> Result<axum::response::Response, ScimError> {
    let list = state.scim.list_groups().await?;
    scim_json(StatusCode::OK, &list)
}

pub async fn get_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ScimError> {
    let group = state.scim.get_group(&id).await?;
    scim_json(StatusCode::OK, &group)
}

pub async fn create_group(
    State(state): State<AppState>,
    Json(body): Json<GroupBodyJson>,
) -> Result<axum::response::Response, ScimError> {
    let (created, group) = state.scim.create_group(body.into()).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    scim_json(status, &group)
}

pub async fn put_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GroupBodyJson>,
) -> Result<axum::response::Response, ScimError> {
    let group = state.scim.put_group(&id, body.into()).await?;
    scim_json(StatusCode::OK, &group)
}

pub async fn delete_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ScimError> {
    state.scim.delete_group(&id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// JSON-deserializable shape for group request bodies.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupBodyJson {
    pub display_name: Option<String>,
    pub external_id: Option<String>,
    pub members: Option<Vec<ScimMember>>,
}

impl From<GroupBodyJson> for GroupBody {
    fn from(j: GroupBodyJson) -> Self {
        Self {
            display_name: j.display_name,
            external_id: j.external_id,
            members: j.members,
        }
    }
}
