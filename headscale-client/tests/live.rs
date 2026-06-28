//! Live integration tests against a real headscale gRPC endpoint.
//!
//! These tests are skipped when `HEADSCALE_TEST_URL` or `HEADSCALE_TEST_API_KEY`
//! are unset, so they are safe to include in the standard test suite — they only
//! run when a live headscale instance is available.
//!
//! To run against the k3d cluster used by `task test-e2e`:
//!
//!   kubectl port-forward -n headmaster-system svc/headscale-server-main 18080:8080 &
//!   KEY=$(kubectl exec -n headmaster-system headscale-server-main-0 -- \
//!         headscale apikeys create --output json | tr -d '"')
//!   HEADSCALE_TEST_URL=http://localhost:18080 HEADSCALE_TEST_API_KEY=$KEY \
//!     cargo test -p headscale-client --test live

use headscale_client::headscale::v1::{
    CreatePreAuthKeyRequest, CreateUserRequest, DeleteUserRequest,
};
use headscale_client::{HeadscaleConnector, LiveConnector};
use prost_types::Timestamp;

fn live_env() -> Option<(String, String)> {
    let url = std::env::var("HEADSCALE_TEST_URL").ok()?;
    let key = std::env::var("HEADSCALE_TEST_API_KEY").ok()?;
    Some((url, key))
}

fn one_hour_from_now() -> Timestamp {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    Timestamp {
        seconds: secs as i64,
        nanos: 0,
    }
}

/// Tags-only pre-auth key (user_id = 0, acl_tags set).
///
/// This is the path exercised when an Ingress has `managed-key-tags` but no
/// `user` annotation. The test confirms whether headscale accepts user_id=0 for
/// tagged keys, which is the assumption the operator relies on.
#[tokio::test]
async fn live_preauth_key_tags_only() {
    let Some((url, api_key)) = live_env() else {
        return;
    };

    let mut client = LiveConnector
        .connect(&url, &api_key)
        .await
        .expect("connect to headscale");

    let result = client
        .create_pre_auth_key(CreatePreAuthKeyRequest {
            user: 0,
            reusable: false,
            ephemeral: false,
            expiration: Some(one_hour_from_now()),
            acl_tags: vec!["tag:server".to_string()],
        })
        .await;

    let key = result
        .unwrap_or_else(|s| panic!("headscale rejected tags-only pre-auth key (user_id=0): {s}"))
        .into_inner()
        .pre_auth_key
        .expect("response must contain a pre_auth_key");

    assert!(!key.key.is_empty(), "key string must be non-empty");
    assert_eq!(
        key.acl_tags,
        vec!["tag:server"],
        "acl_tags must be preserved"
    );
    // user field is None or has id=0 when no user was specified
    let user_id = key.user.as_ref().map(|u| u.id).unwrap_or(0);
    assert_eq!(user_id, 0, "user_id should remain 0 for a tags-only key");
}

/// User + tags pre-auth key (user_id set, acl_tags set).
///
/// Creates a temporary user, creates a key with both user and tags, then
/// cleans up. Confirms the returned key carries both the user and the tags.
#[tokio::test]
async fn live_preauth_key_user_and_tags() {
    let Some((url, api_key)) = live_env() else {
        return;
    };

    let mut client = LiveConnector
        .connect(&url, &api_key)
        .await
        .expect("connect to headscale");

    let user_name = format!(
        "live-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let user = client
        .create_user(CreateUserRequest {
            name: user_name,
            ..Default::default()
        })
        .await
        .expect("create test user")
        .into_inner()
        .user
        .expect("response must contain a user");

    let key = client
        .create_pre_auth_key(CreatePreAuthKeyRequest {
            user: user.id,
            reusable: false,
            ephemeral: false,
            expiration: Some(one_hour_from_now()),
            acl_tags: vec!["tag:server".to_string()],
        })
        .await
        .expect("create pre-auth key with user and tags")
        .into_inner()
        .pre_auth_key
        .expect("response must contain a pre_auth_key");

    assert!(!key.key.is_empty());
    assert_eq!(key.acl_tags, vec!["tag:server"]);
    assert_eq!(
        key.user.as_ref().map(|u| u.id),
        Some(user.id),
        "key must be associated with the created user"
    );

    client
        .delete_user(DeleteUserRequest { id: user.id })
        .await
        .expect("delete test user");
}

/// User-only pre-auth key (user_id set, no acl_tags). Control case.
#[tokio::test]
async fn live_preauth_key_user_only() {
    let Some((url, api_key)) = live_env() else {
        return;
    };

    let mut client = LiveConnector
        .connect(&url, &api_key)
        .await
        .expect("connect to headscale");

    let user_name = format!(
        "live-test2-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let user = client
        .create_user(CreateUserRequest {
            name: user_name,
            ..Default::default()
        })
        .await
        .expect("create test user")
        .into_inner()
        .user
        .expect("response must contain a user");

    let key = client
        .create_pre_auth_key(CreatePreAuthKeyRequest {
            user: user.id,
            reusable: false,
            ephemeral: false,
            expiration: Some(one_hour_from_now()),
            acl_tags: vec![],
        })
        .await
        .expect("create user-only pre-auth key")
        .into_inner()
        .pre_auth_key
        .expect("response must contain a pre_auth_key");

    assert!(!key.key.is_empty());
    assert!(
        key.acl_tags.is_empty(),
        "no tags expected on a user-only key"
    );
    assert_eq!(key.user.as_ref().map(|u| u.id), Some(user.id));

    client
        .delete_user(DeleteUserRequest { id: user.id })
        .await
        .expect("delete test user");
}
