use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::types::SCHEMA_SERVICE_PROVIDER_CONFIG;

const SCIM_JSON: &str = "application/scim+json";

fn scim_json(value: Value) -> impl IntoResponse {
    (
        StatusCode::OK,
        [("Content-Type", SCIM_JSON)],
        serde_json::to_string(&value).unwrap_or_default(),
    )
}

pub async fn service_provider_config() -> impl IntoResponse {
    scim_json(json!({
        "schemas": [SCHEMA_SERVICE_PROVIDER_CONFIG],
        "documentationUri": "",
        "patch": { "supported": false },
        "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
        "filter": { "supported": true },
        "changePassword": { "supported": false },
        "sort": { "supported": false },
        "etag": { "supported": false },
        "authenticationSchemes": [{
            "name": "OAuth Bearer Token",
            "description": "Authentication scheme using the OAuth Bearer Token standard",
            "specUri": "http://www.rfc-editor.org/info/rfc6750",
            "type": "oauthbearertoken",
            "primary": true
        }],
        "meta": {
            "resourceType": "ServiceProviderConfig",
            "location": "/scim/v2/ServiceProviderConfig"
        }
    }))
}

pub async fn schemas() -> impl IntoResponse {
    scim_json(json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
        "totalResults": 2,
        "Resources": [user_schema(), group_schema()]
    }))
}

pub async fn resource_types() -> impl IntoResponse {
    scim_json(json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
        "totalResults": 2,
        "Resources": [
            {
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
                "id": "User",
                "name": "User",
                "endpoint": "/Users",
                "schema": "urn:ietf:params:scim:schemas:core:2.0:User",
                "meta": {
                    "resourceType": "ResourceType",
                    "location": "/scim/v2/ResourceTypes/User"
                }
            },
            {
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
                "id": "Group",
                "name": "Group",
                "endpoint": "/Groups",
                "schema": "urn:ietf:params:scim:schemas:core:2.0:Group",
                "meta": {
                    "resourceType": "ResourceType",
                    "location": "/scim/v2/ResourceTypes/Group"
                }
            }
        ]
    }))
}

fn user_schema() -> Value {
    json!({
        "id": "urn:ietf:params:scim:schemas:core:2.0:User",
        "name": "User",
        "description": "User account",
        "attributes": [
            { "name": "userName", "type": "string", "required": true, "uniqueness": "server" },
            { "name": "displayName", "type": "string", "required": false, "uniqueness": "none" },
            { "name": "emails", "type": "complex", "multiValued": true, "required": false },
            { "name": "active", "type": "boolean", "required": false }
        ],
        "meta": {
            "resourceType": "Schema",
            "location": "/scim/v2/Schemas/urn:ietf:params:scim:schemas:core:2.0:User"
        }
    })
}

fn group_schema() -> Value {
    json!({
        "id": "urn:ietf:params:scim:schemas:core:2.0:Group",
        "name": "Group",
        "description": "Group",
        "attributes": [
            { "name": "displayName", "type": "string", "required": true, "uniqueness": "server" },
            { "name": "members", "type": "complex", "multiValued": true, "required": false }
        ],
        "meta": {
            "resourceType": "Schema",
            "location": "/scim/v2/Schemas/urn:ietf:params:scim:schemas:core:2.0:Group"
        }
    })
}
