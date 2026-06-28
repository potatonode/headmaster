use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::AppState;
use crate::routes::scim_json;
use crate::services::UserBody;
use crate::types::{ScimEmail, ScimError, ScimListResponse};

#[derive(Deserialize)]
pub struct ListQuery {
    pub filter: Option<String>,
}

pub async fn list_users(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<axum::response::Response, ScimError> {
    let username_filter = query
        .filter
        .as_deref()
        .map(parse_username_eq_filter)
        .transpose()
        .map_err(|f| ScimError::not_implemented(format!("unsupported filter: {f}")))?;
    let list = state.scim.list_users().await?;
    if let Some(name) = username_filter {
        let filtered: Vec<_> = list
            .resources
            .into_iter()
            .filter(|u| u.user_name == name)
            .collect();
        return scim_json(StatusCode::OK, &ScimListResponse::new(filtered));
    }
    scim_json(StatusCode::OK, &list)
}

/// Parses `userName eq "value"` (case-insensitive attribute name).
/// Returns `Err(original_filter)` for any other filter expression.
fn parse_username_eq_filter(filter: &str) -> Result<String, &str> {
    let trimmed = filter.trim();
    let lower = trimmed.to_lowercase();
    let Some(rest) = lower.strip_prefix("username eq \"") else {
        return Err(trimmed);
    };
    if !rest.ends_with('"') {
        return Err(trimmed);
    }
    // Use the lowercased string only to locate the attribute-name prefix; slice
    // the value out of the original so its case is preserved.
    let prefix_len = lower.len() - rest.len();
    let value = &trimmed[prefix_len..trimmed.len() - 1];
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_parses_username_eq() {
        assert_eq!(
            parse_username_eq_filter(r#"userName eq "alice""#),
            Ok("alice".to_string())
        );
    }

    #[test]
    fn filter_parses_case_insensitive_attribute() {
        assert_eq!(
            parse_username_eq_filter(r#"USERNAME EQ "alice""#),
            Ok("alice".to_string())
        );
    }

    #[test]
    fn filter_preserves_value_case() {
        assert_eq!(
            parse_username_eq_filter(r#"userName eq "Alice""#),
            Ok("Alice".to_string())
        );
    }

    #[test]
    fn filter_preserves_value_case_with_uppercase_attribute() {
        assert_eq!(
            parse_username_eq_filter(r#"USERNAME EQ "Alice@Example.Com""#),
            Ok("Alice@Example.Com".to_string())
        );
    }

    #[test]
    fn filter_rejects_unsupported_attribute() {
        assert!(parse_username_eq_filter(r#"displayName eq "alice""#).is_err());
    }

    #[test]
    fn filter_rejects_active_filter() {
        assert!(parse_username_eq_filter("active eq true").is_err());
    }
}

pub async fn get_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ScimError> {
    let user = state.scim.get_user(&id).await?;
    scim_json(StatusCode::OK, &user)
}

pub async fn create_user(
    State(state): State<AppState>,
    Json(body): Json<UserBodyJson>,
) -> Result<axum::response::Response, ScimError> {
    let (created, user) = state.scim.create_user(body.into()).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    scim_json(status, &user)
}

pub async fn put_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UserBodyJson>,
) -> Result<axum::response::Response, ScimError> {
    let user = state.scim.put_user(&id, body.into()).await?;
    scim_json(StatusCode::OK, &user)
}

pub async fn delete_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ScimError> {
    state.scim.delete_user(&id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// JSON-deserializable shape for user request bodies.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBodyJson {
    pub user_name: Option<String>,
    pub display_name: Option<String>,
    pub external_id: Option<String>,
    pub emails: Option<Vec<ScimEmail>>,
    pub active: Option<bool>,
}

impl From<UserBodyJson> for UserBody {
    fn from(j: UserBodyJson) -> Self {
        Self {
            user_name: j.user_name,
            display_name: j.display_name,
            external_id: j.external_id,
            emails: j.emails,
            active: j.active,
        }
    }
}
