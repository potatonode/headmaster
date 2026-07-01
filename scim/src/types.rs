use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use headscale_client::policy::PolicyParseError;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

// ── domain newtypes ────────────────────────────────────────────────────────────

/// Stable UUID exposed to SCIM clients for users.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ScimId(pub String);

impl ScimId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScimId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable UUID exposed to SCIM clients for groups.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GroupScimId(pub String);

impl GroupScimId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GroupScimId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Opaque ID assigned by the IdP (e.g. Pocket ID's internal UUID).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalId(pub String);

impl ExternalId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExternalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ── SCIM wire types ────────────────────────────────────────────────────────────

pub const SCHEMA_USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
pub const SCHEMA_GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
pub const SCHEMA_LIST_RESPONSE: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
pub const SCHEMA_ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
pub const SCHEMA_SERVICE_PROVIDER_CONFIG: &str =
    "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScimUser {
    pub schemas: Vec<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    pub user_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub emails: Vec<ScimEmail>,
    pub active: bool,
    pub meta: ScimMeta,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScimEmail {
    pub value: String,
    pub primary: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScimMeta {
    pub resource_type: String,
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<Timestamp>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScimGroup {
    pub schemas: Vec<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    pub display_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub members: Vec<ScimMember>,
    pub meta: ScimMeta,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScimMember {
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ScimListResponse<T: Serialize> {
    pub schemas: Vec<String>,
    pub total_results: usize,
    pub start_index: usize,
    pub items_per_page: usize,
    #[serde(rename = "Resources")]
    pub resources: Vec<T>,
}

impl<T: Serialize> ScimListResponse<T> {
    pub fn new(resources: Vec<T>) -> Self {
        let total = resources.len();
        Self {
            schemas: vec![SCHEMA_LIST_RESPONSE.to_string()],
            total_results: total,
            start_index: 1,
            items_per_page: total,
            resources,
        }
    }
}

#[derive(Debug)]
pub struct ScimError {
    pub status: StatusCode,
    pub detail: String,
}

impl ScimError {
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            detail: detail.into(),
        }
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            detail: detail.into(),
        }
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            detail: detail.into(),
        }
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: detail.into(),
        }
    }

    pub fn not_implemented(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            detail: detail.into(),
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            detail: "Invalid or missing bearer token".to_string(),
        }
    }
}

impl From<PolicyParseError> for ScimError {
    fn from(e: PolicyParseError) -> Self {
        Self::internal(e.to_string())
    }
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "schemas": [SCHEMA_ERROR],
            "detail": self.detail,
            "status": self.status.as_str(),
        });
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        let mut response = (
            self.status,
            [("Content-Type", "application/scim+json")],
            body_str,
        )
            .into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer realm=\"SCIM\""),
            );
        }
        response
    }
}
