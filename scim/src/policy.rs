use std::sync::Arc;

use headscale_client::AuthenticatedClient;
use headscale_client::headscale::v1::{GetPolicyRequest, SetPolicyRequest};
use headscale_client::policy::PolicyEditor;
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
    /// Also prunes stale group members from `grants`: if a `src` or `dst`
    /// entry is a `group:` name that no longer exists in `groups`, that member
    /// is removed from the grant. If removing those members leaves a grant with
    /// an empty `src` or `dst`, the grant itself is removed too. Non-group
    /// members (tags, wildcards, emails) are never touched.
    ///
    /// Skips the `SetPolicy` gRPC call when the resulting policy is semantically
    /// identical to the live one (same JSON values, ignoring whitespace/comments).
    pub async fn set_group_membership(
        &self,
        groups: &[(String, Vec<PolicyMember>)],
    ) -> Result<(), ScimError> {
        let mut client = self.headscale.clone();
        let _guard = self.policy_lock.lock().await;

        // PolicyEditor contains Rc internals and is not Send. The block scope
        // ensures both the fetch await and all PolicyEditor values stay contained:
        // nothing non-Send is live when set_policy is awaited below.
        let new_policy_str = {
            let current = fetch_policy(&mut client).await?;
            let mut new_policy = current.clone();

            new_policy.set_groups(groups);

            // SCIM owns the entire groups section, so any group: member in a
            // grant that isn't in the new groups set is stale — prune it.
            // Non-group members (tags, wildcards, emails) are left untouched.
            let new_groups = new_policy.known_groups();
            for grant in new_policy.grants() {
                let src_was_empty = grant.src().is_empty();
                let dst_was_empty = grant.dst().is_empty();
                for member in grant
                    .src()
                    .iter()
                    .filter(|m| m.starts_with("group:") && !new_groups.contains(*m))
                {
                    grant.remove_from_src(member);
                }
                for member in grant
                    .dst()
                    .iter()
                    .filter(|m| m.starts_with("group:") && !new_groups.contains(*m))
                {
                    grant.remove_from_dst(member);
                }
                // Only remove a grant when WE caused src/dst to become empty.
                // Pre-existing empty sides must not trigger removal.
                let we_emptied_src = !src_was_empty && grant.src().is_empty();
                let we_emptied_dst = !dst_was_empty && grant.dst().is_empty();
                if we_emptied_src || we_emptied_dst {
                    grant.remove();
                }
            }

            if current == new_policy {
                return Ok(());
            }
            new_policy.to_string()
        };

        client
            .set_policy(SetPolicyRequest {
                policy: new_policy_str,
            })
            .await?;
        Ok(())
    }
}

async fn fetch_policy(client: &mut AuthenticatedClient) -> Result<PolicyEditor, ScimError> {
    let policy_str = match client.get_policy(GetPolicyRequest {}).await {
        Ok(resp) => resp.into_inner().policy,
        Err(s) if s.message().contains("policy not found") => String::new(),
        Err(s) => return Err(s.into()),
    };
    PolicyEditor::parse(&policy_str).map_err(ScimError::from)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn email(token: &str) -> PolicyMember {
        PolicyMember {
            token: token.to_string(),
            comment: None,
        }
    }

    // ── set_group_membership integration tests ────────────────────────────────

    #[tokio::test]
    async fn set_group_membership_skips_set_policy_when_semantically_unchanged() {
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

        repo.set_group_membership(&[("eng".to_string(), vec![email("alice@example.com")])])
            .await
            .unwrap();

        assert_eq!(
            *policy_store.lock().unwrap(),
            r#"{"groups":{"group:eng":["alice@example.com"]}}"#,
            "SetPolicy must be skipped when the resulting policy is semantically identical"
        );
    }

    #[tokio::test]
    async fn set_group_membership_calls_set_policy_when_members_change() {
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

        repo.set_group_membership(&[(
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

    #[tokio::test]
    async fn set_group_membership_does_not_remove_preexisting_empty_grant() {
        use headscale_client::AuthInterceptor;
        use headscale_client::HeadscaleServiceClient;
        use headscale_client::fake::{FakeHeadscaleServer, spawn_fake_channel};
        use std::sync::Arc;

        // A grant whose src was already empty before this call (e.g. written
        // directly into the policy by the operator) must not be removed just
        // because its src is empty after pruning.
        let server = FakeHeadscaleServer::default();
        *server.policy.lock().unwrap() = r#"{
            "groups": {"group:eng": ["alice@example.com"]},
            "grants": [
                {"src": [], "dst": ["tag:app"], "ip": ["*:*"]}
            ]
        }"#
        .to_string();
        let policy_store = Arc::clone(&server.policy);
        let channel = spawn_fake_channel(server).await;
        let client =
            HeadscaleServiceClient::with_interceptor(channel, AuthInterceptor::bearer("test"));

        let repo = PolicyRepository::new(client);

        repo.set_group_membership(&[("eng".to_string(), vec![email("alice@example.com")])])
            .await
            .unwrap();

        let stored = policy_store.lock().unwrap().clone();
        let v: serde_json::Value = jsonc_parser::parse_to_serde_value::<serde_json::Value>(
            &stored,
            &jsonc_parser::ParseOptions::default(),
        )
        .unwrap();
        let grants = v["grants"].as_array().expect("grants must be present");
        assert_eq!(
            grants.len(),
            1,
            "grant with pre-existing empty src must not be removed"
        );
    }

    #[tokio::test]
    async fn set_group_membership_does_not_remove_grant_with_preexisting_empty_src_when_dst_is_pruned()
     {
        use headscale_client::AuthInterceptor;
        use headscale_client::HeadscaleServiceClient;
        use headscale_client::fake::{FakeHeadscaleServer, spawn_fake_channel};
        use std::sync::Arc;

        // Grant has src:[] (pre-existing empty) and dst has a stale group plus a
        // live tag. Pruning the stale group from dst must not remove the grant —
        // only WE-caused emptiness on either side triggers removal.
        let server = FakeHeadscaleServer::default();
        *server.policy.lock().unwrap() = r#"{
            "groups": {"group:eng": ["alice@example.com"]},
            "grants": [
                {"src": [], "dst": ["group:stale", "tag:app"], "ip": ["*:*"]}
            ]
        }"#
        .to_string();
        let policy_store = Arc::clone(&server.policy);
        let channel = spawn_fake_channel(server).await;
        let client =
            HeadscaleServiceClient::with_interceptor(channel, AuthInterceptor::bearer("test"));

        let repo = PolicyRepository::new(client);

        repo.set_group_membership(&[("eng".to_string(), vec![email("alice@example.com")])])
            .await
            .unwrap();

        let stored = policy_store.lock().unwrap().clone();
        let v: serde_json::Value = jsonc_parser::parse_to_serde_value::<serde_json::Value>(
            &stored,
            &jsonc_parser::ParseOptions::default(),
        )
        .unwrap();
        let grants = v["grants"].as_array().expect("grants must be present");
        assert_eq!(
            grants.len(),
            1,
            "grant must survive: we pruned dst but src was already empty, not emptied by us"
        );
        assert_eq!(
            grants[0]["dst"][0], "tag:app",
            "stale group must be removed from dst but live tag must remain"
        );
    }

    #[tokio::test]
    async fn set_group_membership_prunes_grants_for_removed_groups() {
        use headscale_client::AuthInterceptor;
        use headscale_client::HeadscaleServiceClient;
        use headscale_client::fake::{FakeHeadscaleServer, spawn_fake_channel};
        use std::sync::Arc;

        let server = FakeHeadscaleServer::default();
        *server.policy.lock().unwrap() = r#"{
            "groups": {"group:eng": ["alice@example.com"], "group:ops": ["bob@example.com"]},
            "grants": [
                {"src": ["group:eng"], "dst": ["tag:app"], "ip": ["*:*"]},
                {"src": ["group:ops"], "dst": ["tag:db"], "ip": ["*:*"]}
            ]
        }"#
        .to_string();
        let policy_store = Arc::clone(&server.policy);
        let channel = spawn_fake_channel(server).await;
        let client =
            HeadscaleServiceClient::with_interceptor(channel, AuthInterceptor::bearer("test"));

        let repo = PolicyRepository::new(client);

        // Remove group:eng, keep group:ops.
        repo.set_group_membership(&[("ops".to_string(), vec![email("bob@example.com")])])
            .await
            .unwrap();

        let stored = policy_store.lock().unwrap().clone();
        let v: serde_json::Value = jsonc_parser::parse_to_serde_value::<serde_json::Value>(
            &stored,
            &jsonc_parser::ParseOptions::default(),
        )
        .unwrap();
        let grants = v["grants"].as_array().expect("grants must be present");
        assert_eq!(grants.len(), 1, "only the ops grant should remain");
        assert_eq!(grants[0]["dst"][0], "tag:db");
    }
}
