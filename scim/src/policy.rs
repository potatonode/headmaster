use std::collections::HashSet;
use std::sync::Arc;

use headscale_client::AuthenticatedClient;
use headscale_client::headscale::v1::{GetPolicyRequest, SetPolicyRequest};
use headscale_client::policy::{PolicyEditor, policies_are_semantically_equal};
use tokio::sync::Mutex;

// Re-export so callers that import from this module keep compiling unchanged.
pub use headscale_client::policy::PolicyMember;

use crate::types::ScimError;

// ── PolicyRepository ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct PolicyRepository {
    headscale: AuthenticatedClient,
    policy_lock: Arc<Mutex<()>>,
}

impl PolicyRepository {
    pub fn new(headscale: AuthenticatedClient) -> Self {
        Self {
            headscale,
            policy_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Replaces the `groups` key in the live headscale policy with the provided
    /// group state. All other policy keys (acls, hosts, tagOwners, etc.) are
    /// preserved. Acquires `policy_lock` for the duration.
    ///
    /// Also removes any `grants` entries that reference a group that is no
    /// longer in `groups` — prevents dangling grants from accumulating when SCIM
    /// removes a group between operator reconciles.
    ///
    /// Skips the `SetPolicy` gRPC call when the resulting policy is semantically
    /// identical to the live one (same JSON values, ignoring whitespace/comments).
    pub async fn reconcile_groups(
        &self,
        groups: &[(String, Vec<PolicyMember>)],
    ) -> Result<(), ScimError> {
        let mut client = self.headscale.clone();
        let _guard = self.policy_lock.lock().await;

        let policy_str = fetch_policy(&mut client).await?;

        let new_policy = build_new_policy(&policy_str, groups)?;

        if policies_are_semantically_equal(policy_str.trim(), new_policy.trim()) {
            return Ok(());
        }

        client
            .set_policy(SetPolicyRequest { policy: new_policy })
            .await?;
        Ok(())
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn build_new_policy(
    policy_str: &str,
    groups: &[(String, Vec<PolicyMember>)],
) -> Result<String, ScimError> {
    let mut editor =
        PolicyEditor::parse(policy_str).map_err(|e| ScimError::internal(e.to_string()))?;

    // Identify which groups are being removed so we can clean up their grants.
    let old_groups: HashSet<String> = editor.known_groups();
    let new_group_names: HashSet<String> = groups
        .iter()
        .map(|(name, _)| format!("group:{name}"))
        .collect();
    let removed_groups: Vec<String> = old_groups.difference(&new_group_names).cloned().collect();

    // Rewrite the groups section.
    editor.set_groups(groups);

    // Remove grants that reference any deleted group so operator-written grants
    // don't dangle when SCIM removes a group between operator reconciles.
    for group in &removed_groups {
        editor.remove_grants_referencing_group(group);
    }

    Ok(editor.to_string())
}

pub(crate) async fn fetch_policy(client: &mut AuthenticatedClient) -> Result<String, ScimError> {
    match client.get_policy(GetPolicyRequest {}).await {
        Ok(resp) => Ok(resp.into_inner().policy),
        Err(s) if s.message().contains("policy not found") => Ok(String::new()),
        Err(s) => Err(s.into()),
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use jsonc_parser::ParseOptions;

    use super::*;

    fn email(token: &str) -> PolicyMember {
        PolicyMember {
            token: token.to_string(),
            comment: None,
        }
    }

    fn ext_id(token: &str, comment: &str) -> PolicyMember {
        PolicyMember {
            token: token.to_string(),
            comment: Some(comment.to_string()),
        }
    }

    fn parse_hujson(s: &str) -> serde_json::Value {
        jsonc_parser::parse_to_serde_value::<serde_json::Value>(s, &ParseOptions::default())
            .unwrap()
    }

    fn parse_json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    // ── set_groups (via PolicyEditor, migrated from set_groups_section) ────────

    #[test]
    fn set_groups_builds_fresh_on_empty_policy() {
        let policy =
            build_new_policy("", &[("eng".to_string(), vec![email("alice@example.com")])]).unwrap();
        let v = parse_json(&policy);
        assert_eq!(v["groups"]["group:eng"][0], "alice@example.com");
    }

    #[test]
    fn set_groups_preserves_other_keys() {
        let policy_str = r#"{"acls": [{"action": "accept"}], "groups": {"group:old": []}}"#;
        let policy = build_new_policy(
            policy_str,
            &[("eng".to_string(), vec![email("alice@example.com")])],
        )
        .unwrap();
        let v = parse_json(&policy);
        assert!(v["acls"].is_array(), "acls must be preserved");
        assert!(v["groups"]["group:old"].is_null(), "old group must be gone");
        assert_eq!(v["groups"]["group:eng"][0], "alice@example.com");
    }

    #[test]
    fn set_groups_with_empty_groups_removes_key() {
        let policy_str = r#"{"groups": {"group:eng": ["alice@example.com"]}}"#;
        let policy = build_new_policy(policy_str, &[]).unwrap();
        let v = parse_json(&policy);
        assert!(
            v["groups"].is_null(),
            "groups key must be absent when list is empty"
        );
    }

    #[test]
    fn set_groups_empty_policy_empty_groups_stays_empty() {
        let policy = build_new_policy("", &[]).unwrap();
        let v = parse_json(&policy);
        assert!(
            v["groups"].is_null(),
            "groups key must not appear when there are no groups"
        );
    }

    #[test]
    fn set_groups_external_id_token_with_block_comment() {
        let policy = build_new_policy(
            "",
            &[(
                "eng".to_string(),
                vec![ext_id(
                    "https://idp.example.com/uuid-1@",
                    "alice@example.com, alice",
                )],
            )],
        )
        .unwrap();
        assert!(
            policy.contains("/* alice@example.com, alice */"),
            "block comment must appear in raw policy output: {policy}"
        );
        let v = parse_hujson(&policy);
        assert_eq!(
            v["groups"]["group:eng"][0],
            "https://idp.example.com/uuid-1@"
        );
    }

    #[test]
    fn set_groups_username_token_no_comment() {
        let policy = build_new_policy("", &[("eng".to_string(), vec![email("alice@")])]).unwrap();
        let v = parse_json(&policy);
        assert_eq!(v["groups"]["group:eng"][0], "alice@");
        assert!(!policy.contains("/*"), "no block comment in Username mode");
    }

    #[test]
    fn set_groups_multiple_members_mixed_comments() {
        let policy = build_new_policy(
            "",
            &[(
                "eng".to_string(),
                vec![
                    ext_id("https://idp/uuid@", "bob@example.com, bob"),
                    email("alice@example.com"),
                ],
            )],
        )
        .unwrap();
        assert!(policy.contains("/* bob@example.com, bob */"));
        let v = parse_hujson(&policy);
        let arr = v["groups"]["group:eng"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "https://idp/uuid@");
        assert_eq!(arr[1], "alice@example.com");
    }

    #[test]
    fn set_groups_token_with_quotes_and_backslashes() {
        let policy = build_new_policy(
            "",
            &[(
                "eng".to_string(),
                vec![ext_id(r#"https://idp/user"name\path@"#, "display name")],
            )],
        )
        .unwrap();
        let v = parse_hujson(&policy);
        assert_eq!(
            v["groups"]["group:eng"][0],
            r#"https://idp/user"name\path@"#
        );
    }

    #[test]
    fn set_groups_comment_with_close_sequence_is_sanitized() {
        let policy = build_new_policy(
            "",
            &[(
                "eng".to_string(),
                vec![ext_id("https://idp/uuid@", "C*/O, alice*/evil")],
            )],
        )
        .unwrap();
        let comment_body = policy
            .split("/* ")
            .nth(1)
            .and_then(|s| s.split(" */").next())
            .unwrap_or("");
        assert!(
            !comment_body.contains("*/"),
            "sanitized comment body must not contain */: {policy}"
        );
        let v = parse_hujson(&policy);
        assert_eq!(v["groups"]["group:eng"][0], "https://idp/uuid@");
    }

    // ── race-condition fix: remove_grants_referencing_group ───────────────────

    #[test]
    fn build_new_policy_removes_grants_for_deleted_groups() {
        let policy_str = r#"{
            "groups": {"group:eng": ["alice@example.com"], "group:ops": ["bob@example.com"]},
            "grants": [
                {"src": ["group:eng"], "dst": ["tag:app"], "ip": ["*:*"]},
                {"src": ["group:ops"], "dst": ["tag:db"], "ip": ["*:*"]}
            ]
        }"#;
        // Remove group:eng but keep group:ops
        let new_policy = build_new_policy(
            policy_str,
            &[("ops".to_string(), vec![email("bob@example.com")])],
        )
        .unwrap();
        let v = parse_hujson(&new_policy);
        let grants = v["grants"].as_array().unwrap();
        assert_eq!(grants.len(), 1, "only the ops grant should remain");
        assert_eq!(grants[0]["dst"][0], "tag:db");
    }

    // ── reconcile_groups integration tests ────────────────────────────────────

    #[tokio::test]
    async fn reconcile_groups_skips_set_policy_when_semantically_unchanged() {
        use headscale_client::AuthInterceptor;
        use headscale_client::HeadscaleServiceClient;
        use headscale_client::fake::{FakeHeadscaleServer, spawn_fake_channel};
        use std::sync::Arc;

        let server = FakeHeadscaleServer::default();
        *server.policy.lock().unwrap() =
            r#"{"groups":{"group:eng":["alice@example.com"]}}"#.to_string();
        let policy_store = Arc::clone(&server.policy);
        let channel = spawn_fake_channel(server).await;
        let client =
            HeadscaleServiceClient::with_interceptor(channel, AuthInterceptor::bearer("test"));

        let repo = PolicyRepository::new(client);

        repo.reconcile_groups(&[("eng".to_string(), vec![email("alice@example.com")])])
            .await
            .unwrap();

        assert_eq!(
            *policy_store.lock().unwrap(),
            r#"{"groups":{"group:eng":["alice@example.com"]}}"#,
            "SetPolicy must be skipped when the resulting policy is semantically identical"
        );
    }

    #[tokio::test]
    async fn reconcile_groups_calls_set_policy_when_members_change() {
        use headscale_client::AuthInterceptor;
        use headscale_client::HeadscaleServiceClient;
        use headscale_client::fake::{FakeHeadscaleServer, spawn_fake_channel};
        use std::sync::Arc;

        let server = FakeHeadscaleServer::default();
        *server.policy.lock().unwrap() =
            r#"{"groups":{"group:eng":["alice@example.com"]}}"#.to_string();
        let policy_store = Arc::clone(&server.policy);
        let channel = spawn_fake_channel(server).await;
        let client =
            HeadscaleServiceClient::with_interceptor(channel, AuthInterceptor::bearer("test"));

        let repo = PolicyRepository::new(client);

        repo.reconcile_groups(&[(
            "eng".to_string(),
            vec![email("alice@example.com"), email("bob@example.com")],
        )])
        .await
        .unwrap();

        let stored = policy_store.lock().unwrap().clone();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(
            v["groups"]["group:eng"].as_array().unwrap().len(),
            2,
            "SetPolicy must be called when members genuinely differ"
        );
    }
}
