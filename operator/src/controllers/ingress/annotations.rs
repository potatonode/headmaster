//! Parses headmaster-specific annotations from `Ingress` objects into typed
//! structs (`IngressAnnotations`, `IngressAccessGrant`) for use by the reconciler.

use std::collections::BTreeMap;

use k8s_openapi::api::networking::v1::Ingress;
use kube::ResourceExt;
use serde::Deserialize;

use super::Error;

pub(crate) const ANNOTATION_CONFIG: &str = "headmaster.potatonode.github.io/config";

const DEFAULT_AUTH_KEY_EXPIRY_SECS: u64 = 600; // 10 minutes

fn default_auth_key_expiry_secs() -> u64 {
    DEFAULT_AUTH_KEY_EXPIRY_SECS
}

/// One entry in the `access` list of the headmaster ingress annotation.
///
/// Each grant specifies a set of source principals and an optional map of
/// app capabilities. When `capabilities` is absent, the grant allows plain
/// IP connectivity (`ip: ["*:*"]`); when present, the grant forwards the
/// listed capabilities to the upstream app via the `Tailscale-App-Capabilities`
/// HTTP header.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct IngressAccessGrant {
    /// Source principals: `group:*`, `tag:*`, `autogroup:*`, `*`, or a user email.
    pub(crate) from: Vec<String>,
    /// Capability name → JSON argument list. If `None`, emits `ip: ["*:*"]`.
    #[serde(default)]
    pub(crate) capabilities: Option<BTreeMap<String, Vec<serde_json::Value>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct IngressConfig {
    headscale_ref: String,
    user: Option<String>,
    #[serde(default)]
    managed_key_tags: Vec<String>,
    hostname: Option<String>,
    #[serde(default = "default_auth_key_expiry_secs")]
    auth_key_expiry_secs: u64,
    #[serde(default)]
    auth_key_reusable: bool,
    #[serde(default)]
    access: Vec<IngressAccessGrant>,
}

pub(crate) struct IngressAnnotations {
    pub(crate) headscale_ref: String,
    pub(crate) user: Option<String>,
    pub(super) managed_key_tags: Vec<String>,
    pub(super) hostname: String,
    pub(super) auth_key_expiry_secs: u64,
    pub(super) auth_key_reusable: bool,
    pub(crate) access: Vec<IngressAccessGrant>,
}

impl IngressAnnotations {
    pub(crate) fn parse(ingress: &Ingress) -> Result<Self, Error> {
        let json = ingress
            .annotations()
            .get(ANNOTATION_CONFIG)
            .ok_or(Error::MissingAnnotation(ANNOTATION_CONFIG))?;
        let cfg: IngressConfig = serde_json::from_str(json)
            .map_err(|e| Error::InvalidAnnotation(ANNOTATION_CONFIG, e.to_string()))?;
        if cfg.user.is_none() && cfg.managed_key_tags.is_empty() {
            return Err(Error::InvalidAnnotations(
                "at least one of 'user' or 'managed-key-tags' must be set",
            ));
        }
        let hostname = cfg.hostname.unwrap_or_else(|| ingress.name_any());
        Ok(Self {
            headscale_ref: cfg.headscale_ref,
            user: cfg.user,
            managed_key_tags: cfg.managed_key_tags,
            hostname,
            auth_key_expiry_secs: cfg.auth_key_expiry_secs,
            auth_key_reusable: cfg.auth_key_reusable,
            access: cfg.access,
        })
    }

    pub(crate) fn headscale_ref(ingress: &Ingress) -> Option<String> {
        let json = ingress.annotations().get(ANNOTATION_CONFIG)?;
        serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|v| {
                v.get("headscale-ref")
                    .and_then(|r| r.as_str())
                    .map(String::from)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn make_test_ingress(user: Option<&str>, tags: Option<&[&str]>) -> Ingress {
        use std::collections::BTreeMap;
        let mut config = serde_json::json!({
            "headscale-ref": "headscale"
        });
        if let Some(u) = user {
            config["user"] = serde_json::Value::String(u.to_string());
        }
        if let Some(t) = tags {
            config["managed-key-tags"] = serde_json::json!(t);
        }
        Ingress {
            metadata: ObjectMeta {
                name: Some("test".to_string()),
                namespace: Some("default".to_string()),
                annotations: Some(BTreeMap::from([(
                    ANNOTATION_CONFIG.to_string(),
                    config.to_string(),
                )])),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn ingress_with_config(extra: serde_json::Value) -> Ingress {
        use std::collections::BTreeMap;
        let mut config = serde_json::json!({
            "headscale-ref": "main",
            "user": "alice"
        });
        if let serde_json::Value::Object(map) = extra {
            for (k, v) in map {
                config[k] = v;
            }
        }
        Ingress {
            metadata: ObjectMeta {
                name: Some("test".to_string()),
                namespace: Some("default".to_string()),
                annotations: Some(BTreeMap::from([(
                    ANNOTATION_CONFIG.to_string(),
                    config.to_string(),
                )])),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn annotation_parse_rejects_neither_user_nor_tags() {
        let ing = make_test_ingress(None, None);
        assert!(
            matches!(
                IngressAnnotations::parse(&ing),
                Err(Error::InvalidAnnotations(_))
            ),
            "parse must fail when neither user nor managed-key-tags is set"
        );
    }

    #[test]
    fn annotation_parse_accepts_tags_without_user() {
        let ing = make_test_ingress(None, Some(&["tag:server"]));
        let parsed = IngressAnnotations::parse(&ing).expect("tags-only must be valid");
        assert!(parsed.user.is_none());
        assert_eq!(parsed.managed_key_tags, vec!["tag:server"]);
    }

    #[test]
    fn annotation_parse_accepts_user_without_tags() {
        let ing = make_test_ingress(Some("alice"), None);
        let parsed = IngressAnnotations::parse(&ing).expect("user-only must be valid");
        assert_eq!(parsed.user.as_deref(), Some("alice"));
        assert!(parsed.managed_key_tags.is_empty());
    }

    #[test]
    fn annotation_parse_accepts_user_and_tags() {
        let ing = make_test_ingress(Some("alice"), Some(&["tag:server"]));
        let parsed = IngressAnnotations::parse(&ing).expect("user+tags must be valid");
        assert_eq!(parsed.user.as_deref(), Some("alice"));
        assert_eq!(parsed.managed_key_tags, vec!["tag:server"]);
    }

    #[test]
    fn annotation_parse_invalid_expiry_is_rejected() {
        let ingress =
            ingress_with_config(serde_json::json!({"auth-key-expiry-secs": "ten-minutes"}));
        assert!(
            matches!(
                IngressAnnotations::parse(&ingress),
                Err(Error::InvalidAnnotation(_, _))
            ),
            "non-numeric auth-key-expiry-secs must be rejected"
        );
    }

    #[test]
    fn annotation_parse_valid_expiry_is_respected() {
        let ingress = ingress_with_config(serde_json::json!({"auth-key-expiry-secs": 3600}));
        let parsed = IngressAnnotations::parse(&ingress).expect("must parse with valid expiry");
        assert_eq!(parsed.auth_key_expiry_secs, 3600);
    }

    #[test]
    fn annotation_parse_defaults_expiry_when_absent() {
        let ingress = ingress_with_config(serde_json::json!({}));
        let parsed = IngressAnnotations::parse(&ingress).expect("must parse without expiry");
        assert_eq!(parsed.auth_key_expiry_secs, DEFAULT_AUTH_KEY_EXPIRY_SECS);
    }

    #[test]
    fn headscale_ref_extracts_from_config() {
        let ing = make_test_ingress(Some("alice"), None);
        assert_eq!(
            IngressAnnotations::headscale_ref(&ing).as_deref(),
            Some("headscale")
        );
    }

    #[test]
    fn annotation_parse_access_empty_by_default() {
        let ing = make_test_ingress(Some("alice"), None);
        let parsed = IngressAnnotations::parse(&ing).expect("must parse");
        assert!(parsed.access.is_empty(), "access must default to empty");
    }

    #[test]
    fn annotation_parse_access_plain_grant() {
        let ingress = ingress_with_config(serde_json::json!({
            "access": [{"from": ["group:eng"]}]
        }));
        let parsed = IngressAnnotations::parse(&ingress).expect("must parse");
        assert_eq!(parsed.access.len(), 1);
        assert_eq!(parsed.access[0].from, vec!["group:eng"]);
        assert!(
            parsed.access[0].capabilities.is_none(),
            "capabilities must be None when absent"
        );
    }

    #[test]
    fn annotation_parse_access_capability_grant() {
        let ingress = ingress_with_config(serde_json::json!({
            "access": [{
                "from": ["group:eng", "alice@example.com"],
                "capabilities": {
                    "myapp/cap/admin": [{"role": "admin"}]
                }
            }]
        }));
        let parsed = IngressAnnotations::parse(&ingress).expect("must parse");
        assert_eq!(parsed.access.len(), 1);
        assert_eq!(parsed.access[0].from.len(), 2);
        let caps = parsed.access[0]
            .capabilities
            .as_ref()
            .expect("capabilities must be Some");
        assert!(caps.contains_key("myapp/cap/admin"));
    }

    #[test]
    fn annotation_parse_access_unknown_field_rejected() {
        let ingress = ingress_with_config(serde_json::json!({
            "access": [{"from": ["group:eng"], "unknown-field": true}]
        }));
        assert!(
            matches!(
                IngressAnnotations::parse(&ingress),
                Err(Error::InvalidAnnotation(_, _))
            ),
            "unknown field in access grant must be rejected"
        );
    }
}
