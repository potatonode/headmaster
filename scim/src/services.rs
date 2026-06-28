use futures::future::try_join_all;
use headscale_client::headscale::v1::{
    DeleteUserRequest, ExpireNodeRequest, ListNodesRequest, ListUsersRequest,
};
use headscale_client::{AuthenticatedClient, Code, Status as GrpcStatus};
use prost_types::Timestamp as PbTimestamp;
use uuid::Uuid;

// Trait impl covers all modules in this crate.
impl From<GrpcStatus> for crate::types::ScimError {
    fn from(s: GrpcStatus) -> Self {
        Self::internal(s.message().to_string())
    }
}

use crate::policy::{PolicyMember, PolicyRepository};
use crate::storage::{GroupRecord, Mapping, SharedMapping, UserRecord};
use crate::types::{
    ExternalId, GroupScimId, SCHEMA_GROUP, SCHEMA_USER, ScimEmail, ScimError, ScimGroup, ScimId,
    ScimListResponse, ScimMember, ScimMeta, ScimUser,
};

// ── config ────────────────────────────────────────────────────────────────────

/// Controls which identifier is written into headscale policy group entries
/// and used to locate a user's headscale account for session management.
#[derive(Clone, Debug, Default)]
pub enum PolicyUserKey {
    /// Write the user's email: "alice@example.com"
    /// Default. Works for any IdP that always provides email (Azure AD, etc.)
    #[default]
    Email,

    /// Write the user's SCIM userName with trailing @: "alice@"
    /// Works for any IdP; stable unless username changes.
    Username,

    /// Write the full OIDC ProviderIdentifier with trailing @:
    /// "https://idp.example.com/uuid@" /* email, username */
    /// Stable across all identifier changes. Requires oidc_issuer config.
    /// Works for Pocket ID, Authentik, Okta (default config).
    ExternalId { oidc_issuer: String },
}

#[derive(Clone, Debug, Default)]
pub struct ScimConfig {
    pub policy_user_key: PolicyUserKey,
    /// When true, expire all of a user's headscale nodes when the identifier
    /// used by policy_user_key changes. Forces immediate OIDC re-auth.
    /// Not needed for ExternalId mode (ProviderIdentifier never changes).
    pub expire_nodes_on_change: bool,
}

/// The headscale-side identifier resolved for a single SCIM user record under
/// the currently configured `PolicyUserKey`. Used to filter headscale's
/// node/user lists and to build policy tokens.
enum ResolvedKey {
    Email(String),
    Name(String),
    /// `oidc_issuer/ext_id` — no trailing `@`. Append `@` for policy tokens.
    ProviderId(String),
}

// ── ScimService ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ScimService {
    headscale: AuthenticatedClient,
    mapping: SharedMapping,
    policy: PolicyRepository,
    config: ScimConfig,
}

impl ScimService {
    pub fn new(headscale: AuthenticatedClient, mapping: SharedMapping, config: ScimConfig) -> Self {
        let policy = PolicyRepository::new(headscale.clone());
        Self {
            headscale,
            mapping,
            policy,
            config,
        }
    }

    // ── users ─────────────────────────────────────────────────────────────────

    pub async fn list_users(&self) -> Result<ScimListResponse<ScimUser>, ScimError> {
        let mapping = self.mapping.lock().await;
        let users = mapping
            .users()
            .map(|(id, r)| user_record_to_scim(id, r))
            .collect();
        Ok(ScimListResponse::new(users))
    }

    pub async fn get_user(&self, id: &str) -> Result<ScimUser, ScimError> {
        let scim_id = ScimId(id.to_string());
        let mapping = self.mapping.lock().await;
        let record = mapping
            .get_user(&scim_id)
            .ok_or_else(|| ScimError::not_found(format!("user {id} not found")))?;
        Ok(user_record_to_scim(&scim_id, record))
    }

    /// Creates a SCIM user record. Does not create a headscale user — OIDC
    /// owns user creation in headscale on first login.
    ///
    /// Treats POST as an upsert when a user with the same `userName` already
    /// exists, mirroring the group behaviour. This makes retries after a failed
    /// `reconcile_groups_policy` idempotent and prevents duplicate records.
    /// Returns `(true, user)` when the user was newly created (HTTP 201 caller),
    /// `(false, user)` when an existing user was updated via upsert (HTTP 200 caller).
    pub async fn create_user(&self, body: UserBody) -> Result<(bool, ScimUser), ScimError> {
        let user_name = body
            .user_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ScimError::bad_request("userName is required"))?
            .to_string();

        if matches!(
            self.config.policy_user_key,
            PolicyUserKey::ExternalId { .. }
        ) && body.external_id.is_none()
        {
            return Err(ScimError::bad_request(
                "externalId is required when policyUserKey=external_id",
            ));
        }

        let new_record = UserRecord {
            active: body.active.unwrap_or(true),
            name: user_name.clone(),
            display_name: body.display_name,
            email: body
                .emails
                .as_deref()
                .and_then(|e| e.first())
                .map(|e| e.value.clone()),
            external_id: body
                .external_id
                .as_deref()
                .map(|s| ExternalId(s.to_string())),
            updated_at: Default::default(),
        };

        let (scim_user, created) = {
            let mut mapping = self.mapping.lock().await;
            let (scim_id, created) = if let Some((existing_id, _)) =
                mapping.get_user_by_name(&user_name)
            {
                // Upsert: user already exists — update so retries after a failed
                // reconcile refresh the record without creating a duplicate.
                tracing::info!(user_name = %user_name, id = %existing_id, "create_user: upsert — updating existing user");
                (existing_id.clone(), false)
            } else {
                (ScimId(Uuid::new_v4().to_string()), true)
            };
            let record = mapping
                .set_user(&scim_id, new_record)
                .await
                .map_err(ScimError::from_io)?;
            (user_record_to_scim(&scim_id, &record), created)
        };
        // Reconcile immediately so any group already referencing this user's SCIM ID
        // gets its policy entry without waiting for a subsequent group write.
        self.reconcile_groups_policy().await?;
        Ok((created, scim_user))
    }

    pub async fn put_user(&self, id: &str, body: UserBody) -> Result<ScimUser, ScimError> {
        let scim_id = ScimId(id.to_string());
        let user_name = body
            .user_name
            .ok_or_else(|| ScimError::bad_request("userName is required"))?;
        let active = body
            .active
            .ok_or_else(|| ScimError::bad_request("active must be set explicitly in a PUT"))?;

        if matches!(
            self.config.policy_user_key,
            PolicyUserKey::ExternalId { .. }
        ) && body.external_id.is_none()
        {
            return Err(ScimError::bad_request(
                "externalId is required when policyUserKey=external_id",
            ));
        }

        let new_email = body
            .emails
            .as_deref()
            .and_then(|e| e.first())
            .map(|e| e.value.clone());

        // Read current state under lock, then release before any I/O. This ensures
        // that on a retry after a failed headscale operation, old_record still
        // reflects the identifier headscale knows the user by — set_user only runs
        // below after a successful headscale operation, so the mapping is never
        // updated with the new identifier until the headscale side is confirmed clean.
        let old_record = {
            let mapping = self.mapping.lock().await;
            mapping
                .get_user(&scim_id)
                .ok_or_else(|| ScimError::not_found(format!("user {id} not found")))?
                .clone()
        };

        let email_changed = new_email != old_record.email;
        let name_changed = user_name != old_record.name;

        // Expire nodes using old identifier values (before any mapping update).
        // For expire_nodes_on_change: only when the configured mode's identifier
        // changes. For deactivation: always (regardless of mode).
        let should_expire_on_change = self.config.expire_nodes_on_change
            && active
            && match &self.config.policy_user_key {
                PolicyUserKey::Email => email_changed,
                PolicyUserKey::Username => name_changed,
                PolicyUserKey::ExternalId { .. } => false,
            };

        if !active || should_expire_on_change {
            self.expire_headscale_user_nodes(&old_record).await?;
        }

        let stored = {
            let mut mapping = self.mapping.lock().await;
            mapping
                .set_user(
                    &scim_id,
                    UserRecord {
                        active,
                        name: user_name,
                        display_name: body.display_name,
                        email: new_email.clone(),
                        external_id: body
                            .external_id
                            .as_deref()
                            .map(|s| ExternalId(s.to_string())),
                        updated_at: Default::default(),
                    },
                )
                .await
                .map_err(ScimError::from_io)?
        };

        // Reconcile after set_user so the policy reflects the current mapping state.
        //
        // We intentionally do NOT gate this on email_changed/name_changed alone.
        // Those are computed from old_record, which is unstable across retries: if
        // set_user succeeded but reconcile failed on the first attempt, a retry reads
        // old_record from the already-updated mapping and sees email_changed=false,
        // silently skipping reconcile and leaving a stale token in the policy.
        //
        // Instead we use conditions that are stable across retries:
        //   !active           — from the request body, never changes between attempts
        //   is_in_any_group   — from the post-set_user mapping; if the user belongs to
        //                       any group their token must appear in policy, so any
        //                       change (email, name, active status) requires reconcile
        let is_in_any_group = {
            let mapping = self.mapping.lock().await;
            mapping.groups().any(|(_, g)| g.members.contains(&scim_id))
        };
        if !active || is_in_any_group {
            self.reconcile_groups_policy().await?;
        }

        Ok(user_record_to_scim(&scim_id, &stored))
    }

    pub async fn delete_user(&self, id: &str) -> Result<(), ScimError> {
        let scim_id = ScimId(id.to_string());

        // Read the record under a short lock, then release before any I/O. A retry
        // after a failed headscale delete can still read the user here because the
        // mapping is not modified until after the delete succeeds.
        let record = {
            let mapping = self.mapping.lock().await;
            mapping
                .get_user(&scim_id)
                .ok_or_else(|| ScimError::not_found(format!("user {id} not found")))?
                .clone()
        };

        // Kill the headscale user before modifying the mapping. If this fails,
        // the error propagates with the mapping intact, so the caller can retry.
        // The lookup is idempotent — not-found is silently ignored — so a second
        // attempt after a successful delete is safe.
        self.delete_headscale_user(&record).await?;

        {
            let mut mapping = self.mapping.lock().await;
            mapping
                .remove_user_and_from_groups(&scim_id)
                .await
                .map_err(ScimError::from_io)?;
        }

        // Rebuild policy with the user removed. If reconcile fails, the mapping is
        // already clean and the next SCIM operation will trigger another reconcile.
        self.reconcile_groups_policy().await?;

        Ok(())
    }

    // ── groups ────────────────────────────────────────────────────────────────

    pub async fn list_groups(&self) -> Result<ScimListResponse<ScimGroup>, ScimError> {
        let mapping = self.mapping.lock().await;
        let groups = mapping
            .groups()
            .map(|(id, r)| group_record_to_scim(id.as_str(), r, &mapping))
            .collect();
        Ok(ScimListResponse::new(groups))
    }

    pub async fn get_group(&self, id: &str) -> Result<ScimGroup, ScimError> {
        let group_id = GroupScimId(id.to_string());
        let mapping = self.mapping.lock().await;
        let record = mapping
            .get_group(&group_id)
            .ok_or_else(|| ScimError::not_found(format!("group '{id}' not found")))?;
        Ok(group_record_to_scim(id, record, &mapping))
    }

    /// Creates or updates (upserts) a SCIM group.
    ///
    /// Returns `(true, group)` when the group was newly created (HTTP 201 caller),
    /// `(false, group)` when an existing group was updated (HTTP 200 caller).
    ///
    /// Pocket ID's SCIM sync always uses POST — it never calls PUT after initial
    /// creation. Treating POST as an upsert lets concurrent syncs and repeated
    /// sync runs keep group membership up to date rather than erroring on 409.
    pub async fn create_group(&self, body: GroupBody) -> Result<(bool, ScimGroup), ScimError> {
        let name = require_display_name(&body)?;
        let member_values: Vec<String> = body
            .members
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|m| m.value.clone())
            .collect();
        tracing::info!(
            group_name = %name,
            member_count = member_values.len(),
            member_values = ?member_values,
            "create_group: received"
        );

        let (scim_id, stored, created) = {
            let mut mapping = self.mapping.lock().await;
            let members = collect_member_ids(body.members.as_deref(), &mapping);
            let (scim_id, created) = if let Some((existing_id, _)) =
                mapping.get_group_by_name(&name)
            {
                // Upsert: group already exists — update it so subsequent Pocket ID
                // syncs can refresh membership without needing PUT support.
                tracing::info!(group_name = %name, id = %existing_id, "create_group: upsert — updating existing group");
                (existing_id.clone(), false)
            } else {
                let scim_id = GroupScimId(Uuid::new_v4().to_string());
                tracing::info!(group_name = %name, id = %scim_id, "create_group: creating new group");
                (scim_id, true)
            };
            let stored = mapping
                .set_group(
                    scim_id.clone(),
                    GroupRecord {
                        name: name.clone(),
                        members,
                        external_id: body
                            .external_id
                            .as_deref()
                            .map(|s| ExternalId(s.to_string())),
                        updated_at: Default::default(),
                    },
                )
                .await
                .map_err(ScimError::from_io)?;
            (scim_id, stored, created)
        };

        self.reconcile_groups_policy().await?;

        let mapping = self.mapping.lock().await;
        Ok((
            created,
            group_record_to_scim(scim_id.as_str(), &stored, &mapping),
        ))
    }

    pub async fn put_group(&self, id: &str, body: GroupBody) -> Result<ScimGroup, ScimError> {
        let new_name = require_display_name(&body)?;
        let group_id = GroupScimId(id.to_string());

        let stored = {
            let mut mapping = self.mapping.lock().await;
            let old_name = mapping
                .get_group(&group_id)
                .ok_or_else(|| ScimError::not_found(format!("group '{id}' not found")))?
                .name
                .clone();
            if new_name != old_name
                && let Some((existing_id, _)) = mapping.get_group_by_name(&new_name)
                && existing_id != &group_id
            {
                return Err(ScimError::conflict(format!(
                    "group '{new_name}' already exists"
                )));
            }
            let members = collect_member_ids(body.members.as_deref(), &mapping);
            mapping
                .set_group(
                    group_id.clone(),
                    GroupRecord {
                        name: new_name.clone(),
                        members,
                        external_id: body
                            .external_id
                            .as_deref()
                            .map(|s| ExternalId(s.to_string())),
                        // Explicit PUT: update timestamp so Pocket ID's next
                        // sync correctly detects no further changes needed.
                        updated_at: jiff::Timestamp::now(),
                    },
                )
                .await
                .map_err(ScimError::from_io)?
        };

        self.reconcile_groups_policy().await?;

        let mapping = self.mapping.lock().await;
        Ok(group_record_to_scim(id, &stored, &mapping))
    }

    pub async fn delete_group(&self, id: &str) -> Result<(), ScimError> {
        let group_id = GroupScimId(id.to_string());
        {
            let mut mapping = self.mapping.lock().await;
            if mapping.get_group(&group_id).is_none() {
                return Err(ScimError::not_found(format!("group '{id}' not found")));
            }
            mapping
                .remove_group(&group_id)
                .await
                .map_err(ScimError::from_io)?;
        }
        self.reconcile_groups_policy().await
    }

    // ── private ───────────────────────────────────────────────────────────────

    /// Resolves the headscale-side identifier for a user record under the
    /// configured `PolicyUserKey`. Returns `None` and logs a warning when the
    /// required identifier field is absent (e.g. Email mode but no email).
    fn resolve_key(&self, record: &UserRecord, caller: &str) -> Option<ResolvedKey> {
        match &self.config.policy_user_key {
            PolicyUserKey::Email => {
                let Some(email) = record.email.as_deref() else {
                    tracing::warn!(
                        name = %record.name,
                        "{caller}: Email mode but no email in record; skipping"
                    );
                    return None;
                };
                Some(ResolvedKey::Email(email.to_string()))
            }
            PolicyUserKey::Username => Some(ResolvedKey::Name(record.name.clone())),
            PolicyUserKey::ExternalId { oidc_issuer } => {
                let Some(ext_id) = &record.external_id else {
                    tracing::warn!(
                        name = %record.name,
                        "{caller}: ExternalId mode but no external_id in record; skipping"
                    );
                    return None;
                };
                Some(ResolvedKey::ProviderId(format!(
                    "{}/{}",
                    oidc_issuer, ext_id.0
                )))
            }
        }
    }

    /// Rebuilds the `groups` section of the headscale policy from the mapping.
    /// Inactive users are excluded from all group entries.
    async fn reconcile_groups_policy(&self) -> Result<(), ScimError> {
        let groups: Vec<(String, Vec<PolicyMember>)> = {
            let mapping = self.mapping.lock().await;
            mapping
                .groups()
                .map(|(_, r)| {
                    let members: Vec<PolicyMember> = r
                        .members
                        .iter()
                        .filter_map(|sid| {
                            let user = mapping.get_user(sid)?;
                            if !user.active {
                                return None;
                            }
                            let key = self.resolve_key(user, "reconcile_groups_policy")?;
                            Some(match key {
                                ResolvedKey::Email(email) => PolicyMember {
                                    token: email,
                                    comment: None,
                                },
                                ResolvedKey::Name(name) => PolicyMember {
                                    token: format!("{name}@"),
                                    comment: None,
                                },
                                ResolvedKey::ProviderId(provider_id) => {
                                    let email = user.email.as_deref().unwrap_or("-");
                                    let raw = format!("{email}, {}", user.name);
                                    PolicyMember {
                                        token: format!("{provider_id}@"),
                                        comment: Some(raw.replace("*/", "* /")),
                                    }
                                }
                            })
                        })
                        .collect();
                    (r.name.clone(), members)
                })
                .collect()
        }; // mapping lock released here, before any network I/O
        self.policy.reconcile_groups(&groups).await
    }

    /// Expires all headscale nodes owned by the user. Uses the pre-change
    /// `record` (old_record in put_user) so the lookup matches headscale's
    /// current state before any identifier update.
    async fn expire_headscale_user_nodes(&self, record: &UserRecord) -> Result<(), ScimError> {
        let Some(key) = self.resolve_key(record, "expire_headscale_user_nodes") else {
            return Ok(());
        };
        let mut client = self.headscale.clone();
        // Fetch all nodes and filter locally to avoid ErrUserNotUnique when
        // using ListNodes(user=name) with non-unique usernames.
        let nodes = client
            .list_nodes(ListNodesRequest {
                user: String::new(),
            })
            .await?
            .into_inner()
            .nodes;

        let node_ids: Vec<u64> = nodes
            .into_iter()
            .filter(|n| {
                n.user.as_ref().is_some_and(|u| match &key {
                    ResolvedKey::Email(e) => u.email == *e,
                    ResolvedKey::Name(n) => u.name == *n,
                    ResolvedKey::ProviderId(id) => u.provider_id == *id,
                })
            })
            .map(|n| n.id)
            .collect();

        try_join_all(node_ids.into_iter().map(|node_id| {
            let mut client = client.clone();
            async move {
                match client
                    .expire_node(ExpireNodeRequest {
                        node_id,
                        // Past timestamp forces immediate session termination.
                        expiry: Some(PbTimestamp {
                            seconds: 1,
                            nanos: 0,
                        }),
                        disable_expiry: false,
                    })
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(s) if s.code() == Code::NotFound => Ok(()),
                    Err(s) => Err(ScimError::from(s)),
                }
            }
        }))
        .await?;
        Ok(())
    }

    /// Finds and deletes the headscale user account for the given record.
    /// Uses ListUsers() with no filter and matches locally.
    async fn delete_headscale_user(&self, record: &UserRecord) -> Result<(), ScimError> {
        let Some(key) = self.resolve_key(record, "delete_headscale_user") else {
            return Ok(());
        };
        let mut client = self.headscale.clone();
        let users = client
            .list_users(ListUsersRequest::default())
            .await?
            .into_inner()
            .users;

        let user = users.into_iter().find(|u| match &key {
            ResolvedKey::Email(e) => u.email == *e,
            ResolvedKey::Name(n) => u.name == *n,
            ResolvedKey::ProviderId(id) => u.provider_id == *id,
        });

        if let Some(user) = user {
            match client.delete_user(DeleteUserRequest { id: user.id }).await {
                Ok(_) => {}
                Err(s) if s.code() == Code::NotFound => {}
                Err(s) => return Err(s.into()),
            }
        }
        Ok(())
    }
}

// ── body types ────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct UserBody {
    pub user_name: Option<String>,
    pub display_name: Option<String>,
    pub external_id: Option<String>,
    pub emails: Option<Vec<ScimEmail>>,
    pub active: Option<bool>,
}

pub struct GroupBody {
    pub display_name: Option<String>,
    pub external_id: Option<String>,
    pub members: Option<Vec<ScimMember>>,
}

// ── conversion helpers ────────────────────────────────────────────────────────

pub(crate) fn user_record_to_scim(scim_id: &ScimId, record: &UserRecord) -> ScimUser {
    ScimUser {
        schemas: vec![SCHEMA_USER.to_string()],
        id: scim_id.as_str().to_string(),
        external_id: record.external_id.as_ref().map(|e| e.as_str().to_string()),
        user_name: record.name.clone(),
        display_name: record.display_name.clone(),
        emails: record
            .email
            .as_ref()
            .map(|e| {
                vec![ScimEmail {
                    value: e.clone(),
                    primary: true,
                }]
            })
            .unwrap_or_default(),
        active: record.active,
        meta: ScimMeta {
            resource_type: "User".to_string(),
            location: format!("/scim/v2/Users/{scim_id}"),
            last_modified: Some(record.updated_at),
        },
    }
}

pub(crate) fn group_record_to_scim(id: &str, record: &GroupRecord, mapping: &Mapping) -> ScimGroup {
    let members: Vec<ScimMember> = record
        .members
        .iter()
        .filter_map(|scim_id| {
            let user = mapping.get_user(scim_id)?;
            Some(ScimMember {
                value: scim_id.as_str().to_string(),
                display: user.email.clone(),
            })
        })
        .collect();
    ScimGroup {
        schemas: vec![SCHEMA_GROUP.to_string()],
        id: id.to_string(),
        external_id: record.external_id.as_ref().map(|e| e.as_str().to_string()),
        display_name: record.name.clone(),
        members,
        meta: ScimMeta {
            resource_type: "Group".to_string(),
            location: format!("/scim/v2/Groups/{id}"),
            last_modified: Some(record.updated_at),
        },
    }
}

fn require_display_name(body: &GroupBody) -> Result<String, ScimError> {
    body.display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ScimError::bad_request("displayName is required"))
}

/// Collects SCIM member UUIDs for storage in a group record.
///
/// Unknown member IDs (users not yet in our mapping) are stored as-is and
/// resolved at policy-reconciliation time once those users are synced. This
/// tolerates the race between a SCIM sync's user-creation pass and its
/// group-creation pass when both run concurrently.
///
/// Inactive users and users without the configured identifier are also stored;
/// `reconcile_groups_policy` filters them from the headscale policy, so they
/// never appear in enforcement.
fn collect_member_ids(members: Option<&[ScimMember]>, mapping: &Mapping) -> Vec<ScimId> {
    let Some(members) = members else {
        return Vec::new();
    };
    members
        .iter()
        .map(|m| {
            if mapping.get_user(&ScimId(m.value.clone())).is_none() {
                tracing::warn!(
                    member_value = %m.value,
                    "group member not yet in mapping; storing for deferred resolution"
                );
            }
            ScimId(m.value.clone())
        })
        .collect()
}

impl ScimError {
    fn from_io(e: std::io::Error) -> Self {
        Self::internal(e.to_string())
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use headscale_client::AuthInterceptor;
    use headscale_client::HeadscaleServiceClient;
    use headscale_client::fake::{FakeHeadscaleServer, spawn_fake_channel};
    use headscale_client::headscale::v1::User;

    use super::*;
    use crate::storage;

    async fn make_service(
        server: FakeHeadscaleServer,
    ) -> (ScimService, storage::SharedMapping, tempfile::TempDir) {
        make_service_with_config(server, ScimConfig::default()).await
    }

    async fn make_service_with_config(
        server: FakeHeadscaleServer,
        config: ScimConfig,
    ) -> (ScimService, storage::SharedMapping, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");
        let shared = storage::load_shared(&path).await.unwrap();
        let channel = spawn_fake_channel(server).await;
        let client =
            HeadscaleServiceClient::with_interceptor(channel, AuthInterceptor::bearer("test"));
        let svc = ScimService::new(client, shared.clone(), config);
        (svc, shared, dir)
    }

    async fn create_alice(svc: &ScimService) -> ScimUser {
        svc.create_user(UserBody {
            user_name: Some("alice".to_string()),
            emails: Some(vec![ScimEmail {
                value: "alice@example.com".to_string(),
                primary: true,
            }]),
            external_id: Some("ext-alice".to_string()),
            ..Default::default()
        })
        .await
        .expect("create alice")
        .1
    }

    // ── user tests ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_returns_uuid_and_active_true() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let user = create_alice(&svc).await;
        assert!(uuid::Uuid::parse_str(&user.id).is_ok());
        assert!(user.active);
        assert_eq!(user.user_name, "alice");
    }

    #[tokio::test]
    async fn create_does_not_call_headscale() {
        // Fake server has no users — if create_user gRPC was called it would
        // succeed but we verify no headscale user was created.
        let server = FakeHeadscaleServer::default();
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await;
        create_alice(&svc).await;
        assert!(
            headscale_state.lock().unwrap().users.is_empty(),
            "create_user must not call headscale gRPC"
        );
    }

    #[tokio::test]
    async fn get_user_by_scim_id() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let created = create_alice(&svc).await;
        let fetched = svc.get_user(&created.id).await.unwrap();
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.user_name, "alice");
    }

    #[tokio::test]
    async fn create_user_duplicate_username_upserts() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;

        let (first_created, first) = svc
            .create_user(UserBody {
                user_name: Some("alice".to_string()),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                external_id: Some("ext-alice".to_string()),
                ..Default::default()
            })
            .await
            .expect("first create_user must succeed");
        assert!(first_created, "first POST must report created=true");

        // Second POST with the same userName — must upsert, not create a duplicate.
        let (second_created, second) = svc
            .create_user(UserBody {
                user_name: Some("alice".to_string()),
                emails: Some(vec![ScimEmail {
                    value: "alice@new.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            })
            .await
            .expect("duplicate create_user must succeed as upsert");
        assert!(!second_created, "upsert POST must report created=false");

        assert_eq!(
            first.id, second.id,
            "upsert must preserve the existing SCIM ID"
        );
        assert_eq!(
            second.emails[0].value, "alice@new.com",
            "email must be updated"
        );

        let list = svc.list_users().await.unwrap();
        assert_eq!(
            list.resources.len(),
            1,
            "must not create a duplicate record"
        );
    }

    #[tokio::test]
    async fn put_active_false_marks_inactive_and_expires_nodes() {
        // Deactivation must expire all headscale nodes owned by the user (Email mode).
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "alice".to_string(),
            email: "alice@example.com".to_string(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await;

        let created = create_alice(&svc).await;

        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("alice".to_string()),
                    active: Some(false),
                    emails: Some(vec![ScimEmail {
                        value: "alice@example.com".to_string(),
                        primary: true,
                    }]),
                    ..Default::default()
                },
            )
            .await
            .expect("deactivation must succeed");

        assert!(!result.active);
        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_some(),
            "node must have expiry set after deactivation"
        );
    }

    #[tokio::test]
    async fn put_active_false_retry_still_expires_nodes() {
        // A retry PUT active=false must still expire nodes even when
        // old_record.active is already false (idempotent expiry).
        let server = FakeHeadscaleServer::default();
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await;

        let created = create_alice(&svc).await;

        // First deactivation — succeeds end-to-end.
        svc.put_user(
            &created.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(false),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .expect("first deactivation must succeed");

        // Inject a fresh node (simulating re-registration after the first expiry).
        headscale_state
            .lock()
            .unwrap()
            .nodes
            .push(headscale_client::headscale::v1::Node {
                id: 99,
                user: Some(User {
                    id: 1,
                    name: "alice".to_string(),
                    email: "alice@example.com".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            });

        // Retry: alice is already inactive in the mapping. Must still expire the new node.
        svc.put_user(
            &created.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(false),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .expect("retry deactivation must succeed");

        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == 99).unwrap();
        assert!(
            node.expiry.is_some(),
            "re-injected node must be expired on retry deactivation"
        );
    }

    #[tokio::test]
    async fn put_active_false_with_email_change_expires_by_old_email() {
        // Deactivating while simultaneously changing the email must expire
        // the headscale node, which is registered under the old email.
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "alice".to_string(),
            email: "alice@example.com".to_string(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await;

        let created = create_alice(&svc).await;

        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("alice".to_string()),
                    active: Some(false),
                    emails: Some(vec![ScimEmail {
                        value: "alice@new.com".to_string(),
                        primary: true,
                    }]),
                    ..Default::default()
                },
            )
            .await
            .expect("deactivation with email change must succeed");

        assert!(!result.active);
        assert_eq!(
            result.emails[0].value, "alice@new.com",
            "email must be updated"
        );
        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_some(),
            "node must be expired using old email when email changes on deactivation"
        );
    }

    #[tokio::test]
    async fn put_active_false_email_change_retry_uses_old_email() {
        // Simulates the retry scenario: the first PUT (email change + deactivation)
        // fails when expiring nodes. Because the mapping is only updated after a
        // successful expire, the retry finds the old email in the mapping and
        // correctly expires the node on the second attempt.
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "alice".to_string(),
            email: "alice@example.com".to_string(),
            ..Default::default()
        });
        let expire_node_fails = Arc::clone(&server.expire_node_fails);
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await;

        let created = create_alice(&svc).await;

        // Arm: headscale expire_node will fail.
        expire_node_fails.store(true, std::sync::atomic::Ordering::Relaxed);

        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("alice".to_string()),
                    active: Some(false),
                    emails: Some(vec![ScimEmail {
                        value: "alice@new.com".to_string(),
                        primary: true,
                    }]),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            result.is_err(),
            "first PUT must fail when headscale expire_node fails"
        );

        // Node must not be expired yet.
        {
            let state = headscale_state.lock().unwrap();
            let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
            assert!(node.expiry.is_none(), "node must not be expired yet");
        }

        // The mapping must not have been updated — the old email must still be present.
        let user = svc.get_user(&created.id).await.unwrap();
        assert_eq!(
            user.emails.first().map(|e| e.value.as_str()),
            Some("alice@example.com"),
            "mapping must not be updated when expire_node fails"
        );
        assert!(
            user.active,
            "user must still be active when expire_node fails"
        );

        // Disarm: let the retry succeed.
        expire_node_fails.store(false, std::sync::atomic::Ordering::Relaxed);

        // Retry: mapping still has old email, so expire finds the node.
        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("alice".to_string()),
                    active: Some(false),
                    emails: Some(vec![ScimEmail {
                        value: "alice@new.com".to_string(),
                        primary: true,
                    }]),
                    ..Default::default()
                },
            )
            .await
            .expect("retry PUT must succeed");

        assert!(!result.active);
        assert_eq!(result.emails[0].value, "alice@new.com");
        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_some(),
            "node must be expired on retry using the old email from the mapping"
        );
    }

    #[tokio::test]
    async fn put_user_email_change_retry_still_reconciles_when_set_user_already_ran() {
        // Regression test for: if set_user succeeded but reconcile_groups_policy failed
        // on the first PUT, a retry must still reconcile even though old_record.email
        // already matches new_email (email_changed=false). Without the fix, the retry
        // silently skipped reconcile and left the old email token in the policy.
        let server = FakeHeadscaleServer::default();
        let set_policy_fails = Arc::clone(&server.set_policy_fails);
        let policy_store = Arc::clone(&server.policy);
        let (svc, _, _dir) = make_service(server).await;

        let alice = create_alice(&svc).await;

        // Put alice into a group so she appears in the policy.
        svc.create_group(GroupBody {
            display_name: Some("eng".to_string()),
            external_id: None,
            members: Some(vec![ScimMember {
                value: alice.id.clone(),
                display: None,
            }]),
        })
        .await
        .unwrap();

        // Confirm alice's old email is in the policy.
        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            policy["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "alice@example.com"),
            "alice's old email must be in the policy before the test"
        );

        // Arm: SetPolicy will fail so the first PUT fails after set_user succeeds.
        set_policy_fails.store(true, std::sync::atomic::Ordering::Relaxed);

        let result = svc
            .put_user(
                &alice.id,
                UserBody {
                    user_name: Some("alice".to_string()),
                    active: Some(true),
                    emails: Some(vec![ScimEmail {
                        value: "alice@new.com".to_string(),
                        primary: true,
                    }]),
                    ..Default::default()
                },
            )
            .await;
        assert!(result.is_err(), "first PUT must fail when SetPolicy fails");

        // The mapping must already have the new email (set_user ran before reconcile).
        let user = svc.get_user(&alice.id).await.unwrap();
        assert_eq!(
            user.emails[0].value, "alice@new.com",
            "mapping must have new email"
        );

        // Disarm and retry.
        set_policy_fails.store(false, std::sync::atomic::Ordering::Relaxed);

        svc.put_user(
            &alice.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(true),
                emails: Some(vec![ScimEmail {
                    value: "alice@new.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .expect("retry PUT must succeed");

        // The policy must now reflect the new email — not the stale old one.
        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        let members = policy["groups"]["group:eng"].as_array().unwrap();
        assert!(
            members.iter().any(|v| v == "alice@new.com"),
            "policy must have new email after retry reconcile"
        );
        assert!(
            !members.iter().any(|v| v == "alice@example.com"),
            "stale old email must be gone from policy after retry reconcile"
        );
    }

    #[tokio::test]
    async fn put_active_false_no_nodes_is_ok() {
        // User deactivated before ever logging in via OIDC — no headscale nodes exist.
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let created = create_alice(&svc).await;
        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("alice".to_string()),
                    active: Some(false),
                    emails: Some(vec![ScimEmail {
                        value: "alice@example.com".to_string(),
                        primary: true,
                    }]),
                    ..Default::default()
                },
            )
            .await
            .expect("deactivation with no nodes must succeed");
        assert!(!result.active);
    }

    #[tokio::test]
    async fn delete_user_removes_mapping_and_headscale_user() {
        let server = FakeHeadscaleServer::default();
        server.state.lock().unwrap().users.push(User {
            id: 1,
            name: "alice".to_string(),
            email: "alice@example.com".to_string(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await;
        let created = create_alice(&svc).await;

        svc.delete_user(&created.id)
            .await
            .expect("delete must succeed");

        assert!(
            svc.get_user(&created.id).await.is_err(),
            "mapping entry must be gone"
        );
        assert!(
            headscale_state.lock().unwrap().users.is_empty(),
            "headscale user must be deleted"
        );
    }

    #[tokio::test]
    async fn delete_user_deletes_headscale_user_even_when_reconcile_fails() {
        // Simulates a partial failure: policy reconcile (SetPolicy) fails, but the
        // headscale user delete must already have run before reconcile is attempted.
        // delete_user should return an error (reconcile failed), and the headscale
        // user must be gone — not leaked — regardless.
        //
        // Alice must be in a group so reconcile_groups_policy actually needs to call
        // SetPolicy (to remove her from the group). Without group membership the policy
        // is unchanged and SetPolicy would be skipped entirely, never hitting the fault.
        let server = FakeHeadscaleServer::default();
        let set_policy_fails = Arc::clone(&server.set_policy_fails);
        server.state.lock().unwrap().users.push(User {
            id: 1,
            name: "alice".to_string(),
            email: "alice@example.com".to_string(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await;
        let created = create_alice(&svc).await;

        // Put alice in a group so delete triggers a real policy update.
        svc.create_group(GroupBody {
            display_name: Some("eng".to_string()),
            members: Some(vec![ScimMember {
                value: created.id.clone(),
                display: None,
            }]),
            external_id: None,
        })
        .await
        .expect("create group with alice as member");

        // Now arm the failure so only delete_user's reconcile call fails.
        set_policy_fails.store(true, std::sync::atomic::Ordering::Relaxed);

        // delete_user fails because SetPolicy errors, but the headscale user must
        // have been deleted before reconcile was attempted.
        let result = svc.delete_user(&created.id).await;
        assert!(result.is_err(), "delete must propagate the reconcile error");
        assert!(
            headscale_state.lock().unwrap().users.is_empty(),
            "headscale user must be deleted before reconcile is attempted"
        );
    }

    #[tokio::test]
    async fn delete_user_mapping_unchanged_when_headscale_delete_fails() {
        // If delete_headscale_user fails, the mapping must not have been
        // modified — the user must still be findable so a retry can attempt the
        // headscale delete again with the correct identifier.
        let server = FakeHeadscaleServer::default();
        server.state.lock().unwrap().users.push(User {
            id: 1,
            name: "alice".to_string(),
            email: "alice@example.com".to_string(),
            ..Default::default()
        });
        let delete_user_fails = Arc::clone(&server.delete_user_fails);
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await;
        let created = create_alice(&svc).await;

        // Arm: headscale delete will fail.
        delete_user_fails.store(true, std::sync::atomic::Ordering::Relaxed);

        let result = svc.delete_user(&created.id).await;
        assert!(result.is_err(), "delete must propagate the headscale error");
        assert_eq!(
            headscale_state.lock().unwrap().users.len(),
            1,
            "headscale user must still exist when delete fails"
        );
        assert!(
            svc.get_user(&created.id).await.is_ok(),
            "user must still be in the mapping when headscale delete fails (enables retry)"
        );

        // Disarm and retry — both the headscale session and mapping must now be cleaned up.
        delete_user_fails.store(false, std::sync::atomic::Ordering::Relaxed);

        svc.delete_user(&created.id)
            .await
            .expect("retry delete must succeed");

        assert!(
            headscale_state.lock().unwrap().users.is_empty(),
            "headscale user must be deleted on retry"
        );
        assert!(
            svc.get_user(&created.id).await.is_err(),
            "user must be gone from mapping after successful retry"
        );
    }

    // ── group tests ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_groups_reads_from_mapping() {
        let (svc, mapping, _dir) = make_service(FakeHeadscaleServer::default()).await;
        mapping
            .lock()
            .await
            .set_group(
                GroupScimId("gid-1".to_string()),
                GroupRecord {
                    name: "eng".to_string(),
                    members: vec![],
                    external_id: None,
                    updated_at: Default::default(),
                },
            )
            .await
            .unwrap();

        let list = svc.list_groups().await.unwrap();
        assert_eq!(list.resources.len(), 1);
        assert_eq!(list.resources[0].display_name, "eng");
    }

    #[tokio::test]
    async fn inactive_user_excluded_from_group_policy() {
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, _, _dir) = make_service(server).await;

        let alice = create_alice(&svc).await;

        let _ = svc
            .create_group(GroupBody {
                display_name: Some("eng".to_string()),
                external_id: None,
                members: Some(vec![ScimMember {
                    value: alice.id.clone(),
                    display: None,
                }]),
            })
            .await
            .unwrap();

        // Verify alice is in the policy.
        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            policy["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "alice@example.com")
        );

        // Deactivate alice.
        svc.put_user(
            &alice.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(false),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Alice must be removed from the policy.
        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            !policy["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "alice@example.com"),
            "inactive user must be excluded from policy"
        );
    }

    #[tokio::test]
    async fn put_group_renames_group_in_policy_and_mapping() {
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, mapping, _dir) = make_service(server).await;

        mapping
            .lock()
            .await
            .set_group(
                GroupScimId("gid-1".to_string()),
                GroupRecord {
                    name: "eng".to_string(),
                    members: vec![],
                    external_id: None,
                    updated_at: Default::default(),
                },
            )
            .await
            .unwrap();

        let result = svc
            .put_group(
                "gid-1",
                GroupBody {
                    display_name: Some("engineering".to_string()),
                    external_id: None,
                    members: None,
                },
            )
            .await
            .expect("rename must succeed");

        assert_eq!(result.display_name, "engineering");

        let live: serde_json::Value = serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(live["groups"]["group:engineering"].is_array());
        assert!(live["groups"]["group:eng"].is_null());
    }

    #[tokio::test]
    async fn put_user_email_change_rebuilds_group_policy() {
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, mapping, _dir) = make_service(server).await;

        let alice = create_alice(&svc).await;
        mapping
            .lock()
            .await
            .set_group(
                GroupScimId("gid-1".to_string()),
                GroupRecord {
                    name: "eng".to_string(),
                    members: vec![ScimId(alice.id.clone())],
                    external_id: None,
                    updated_at: Default::default(),
                },
            )
            .await
            .unwrap();

        svc.put_user(
            &alice.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(true),
                emails: Some(vec![ScimEmail {
                    value: "alice@new.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let live: serde_json::Value = serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        let members = live["groups"]["group:eng"].as_array().unwrap();
        assert!(members.iter().any(|v| v == "alice@new.com"));
        assert!(!members.iter().any(|v| v == "alice@example.com"));
    }

    #[tokio::test]
    async fn list_users_returns_all_users() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        create_alice(&svc).await;
        svc.create_user(UserBody {
            user_name: Some("bob".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

        let list = svc.list_users().await.unwrap();
        assert_eq!(list.resources.len(), 2);
    }

    #[tokio::test]
    async fn get_user_not_found_returns_404() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let result = svc.get_user("nonexistent-id").await;
        assert!(
            matches!(result, Err(ref e) if e.status == axum::http::StatusCode::NOT_FOUND),
            "must return 404"
        );
    }

    #[tokio::test]
    async fn create_user_missing_username_returns_400() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let result = svc
            .create_user(UserBody {
                user_name: None,
                ..Default::default()
            })
            .await;
        assert!(
            matches!(result, Err(ref e) if e.status == axum::http::StatusCode::BAD_REQUEST),
            "must return 400"
        );
    }

    #[tokio::test]
    async fn put_user_missing_active_returns_400() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let created = create_alice(&svc).await;
        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("alice".to_string()),
                    active: None,
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(result, Err(ref e) if e.status == axum::http::StatusCode::BAD_REQUEST),
            "must return 400"
        );
    }

    #[tokio::test]
    async fn put_user_missing_username_returns_400() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let created = create_alice(&svc).await;
        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: None,
                    active: Some(true),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(result, Err(ref e) if e.status == axum::http::StatusCode::BAD_REQUEST),
            "must return 400"
        );
    }

    #[tokio::test]
    async fn reactivation_restores_user_in_group_policy() {
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, _, _dir) = make_service(server).await;

        let alice = create_alice(&svc).await;
        let _ = svc
            .create_group(GroupBody {
                display_name: Some("eng".to_string()),
                external_id: None,
                members: Some(vec![ScimMember {
                    value: alice.id.clone(),
                    display: None,
                }]),
            })
            .await
            .unwrap();

        // Deactivate alice.
        svc.put_user(
            &alice.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(false),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            !policy["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "alice@example.com"),
            "alice must be absent after deactivation"
        );

        // Reactivate alice.
        svc.put_user(
            &alice.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(true),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            policy["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "alice@example.com"),
            "alice must be restored in policy after reactivation"
        );
    }

    #[tokio::test]
    async fn delete_user_with_no_email_succeeds() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let (_, created) = svc
            .create_user(UserBody {
                user_name: Some("no-email".to_string()),
                emails: None,
                ..Default::default()
            })
            .await
            .unwrap();
        svc.delete_user(&created.id)
            .await
            .expect("delete with no email must not error");
        assert!(svc.get_user(&created.id).await.is_err());
    }

    #[tokio::test]
    async fn create_group_duplicate_name_upserts() {
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, _, _dir) = make_service(server).await;

        let alice = create_alice(&svc).await;

        let (created1, first) = svc
            .create_group(GroupBody {
                display_name: Some("eng".to_string()),
                external_id: None,
                members: None,
            })
            .await
            .unwrap();
        assert!(created1, "first POST must report created=true");

        // Second POST with the same name: upsert with alice as member.
        let (created2, second) = svc
            .create_group(GroupBody {
                display_name: Some("eng".to_string()),
                external_id: None,
                members: Some(vec![ScimMember {
                    value: alice.id.clone(),
                    display: None,
                }]),
            })
            .await
            .unwrap();
        assert!(!created2, "second POST must report created=false (upsert)");
        assert_eq!(
            first.id, second.id,
            "upsert must preserve the existing group's SCIM ID"
        );

        let live: serde_json::Value = serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            live["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "alice@example.com"),
            "upsert must update group members and rebuild policy"
        );
    }

    #[tokio::test]
    async fn delete_group_removes_from_policy() {
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, _, _dir) = make_service(server).await;

        let (_, group) = svc
            .create_group(GroupBody {
                display_name: Some("eng".to_string()),
                external_id: None,
                members: None,
            })
            .await
            .unwrap();

        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            policy["groups"]["group:eng"].is_array(),
            "group must exist after create"
        );

        svc.delete_group(&group.id).await.unwrap();

        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            policy["groups"]["group:eng"].is_null(),
            "group must be absent from policy after delete"
        );
    }

    #[tokio::test]
    async fn create_group_with_inactive_member_excluded_from_policy() {
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, _, _dir) = make_service(server).await;
        let alice = create_alice(&svc).await;

        svc.put_user(
            &alice.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(false),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Inactive member is accepted (no error), but excluded from the policy.
        svc.create_group(GroupBody {
            display_name: Some("eng".to_string()),
            external_id: None,
            members: Some(vec![ScimMember {
                value: alice.id.clone(),
                display: None,
            }]),
        })
        .await
        .expect("create_group with inactive member must succeed");

        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            !policy["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "alice@example.com"),
            "inactive user must be excluded from policy"
        );
    }

    #[tokio::test]
    async fn delete_user_removes_them_from_group_members() {
        let (svc, _, _dir) = make_service(FakeHeadscaleServer::default()).await;
        let alice = create_alice(&svc).await;
        let (_, group) = svc
            .create_group(GroupBody {
                display_name: Some("eng".to_string()),
                external_id: None,
                members: Some(vec![ScimMember {
                    value: alice.id.clone(),
                    display: None,
                }]),
            })
            .await
            .unwrap();

        svc.delete_user(&alice.id).await.unwrap();

        let fetched = svc.get_group(&group.id).await.unwrap();
        assert!(
            fetched.members.is_empty(),
            "group members must be empty after the member user is deleted"
        );
    }

    #[tokio::test]
    async fn create_group_succeeds_when_headscale_has_no_policy_yet() {
        // Simulates a fresh headscale instance where GetPolicy returns NOT_FOUND.
        let server = FakeHeadscaleServer::with_policy_not_found();
        let (svc, _, _dir) = make_service(server).await;
        svc.create_group(GroupBody {
            display_name: Some("eng".to_string()),
            external_id: None,
            members: None,
        })
        .await
        .expect("create_group must succeed even when headscale has no policy set");
    }

    #[tokio::test]
    async fn create_group_with_no_email_member_excluded_from_policy() {
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, _, _dir) = make_service(server).await;
        let (_, no_email) = svc
            .create_user(UserBody {
                user_name: Some("no-email".to_string()),
                emails: None,
                ..Default::default()
            })
            .await
            .unwrap();

        // Member with no email is accepted but excluded from the headscale policy
        // (Email mode requires an email address).
        svc.create_group(GroupBody {
            display_name: Some("eng".to_string()),
            external_id: None,
            members: Some(vec![ScimMember {
                value: no_email.id.clone(),
                display: None,
            }]),
        })
        .await
        .expect("create_group with no-email member must succeed");

        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert_eq!(
            policy["groups"]["group:eng"].as_array().unwrap().len(),
            0,
            "member with no email must be excluded from policy in Email mode"
        );
    }

    #[tokio::test]
    async fn create_group_with_unknown_member_id_resolved_after_user_created() {
        // Simulates the race during concurrent SCIM syncs: the group is written
        // BEFORE the user exists in our mapping. The user is then synced and
        // create_user triggers reconcile, so the policy is updated immediately.
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let (svc, _, _dir) = make_service(server).await;

        let phantom_id = "00000000-0000-0000-0000-000000000042";

        // Group created with a member ID that doesn't exist yet — accepted, policy empty.
        svc.create_group(GroupBody {
            display_name: Some("eng".to_string()),
            external_id: None,
            members: Some(vec![ScimMember {
                value: phantom_id.to_string(),
                display: None,
            }]),
        })
        .await
        .expect("create_group with unknown member must succeed");

        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert_eq!(
            policy["groups"]["group:eng"].as_array().unwrap().len(),
            0,
            "unknown member cannot contribute to policy before user is synced"
        );

        // Now create the user with the SCIM ID that was referenced.
        svc.create_user(UserBody {
            user_name: Some("phantom".to_string()),
            emails: Some(vec![ScimEmail {
                value: "phantom@example.com".to_string(),
                primary: true,
            }]),
            ..Default::default()
        })
        .await
        .expect("create_user must succeed");

        // Hack: the user above was assigned a random UUID, not phantom_id.
        // To test deferred resolution properly we need to insert a user with
        // the exact ID stored in the group.  Bypass via the mapping directly.
        use crate::storage::UserRecord;
        svc.mapping
            .lock()
            .await
            .set_user(
                &ScimId(phantom_id.to_string()),
                UserRecord {
                    active: true,
                    name: "phantom".to_string(),
                    display_name: None,
                    email: Some("phantom@example.com".to_string()),
                    external_id: None,
                    updated_at: Default::default(),
                },
            )
            .await
            .unwrap();
        // Manually trigger reconcile (simulating what create_user does for the right ID).
        svc.reconcile_groups_policy()
            .await
            .expect("reconcile must succeed");

        let policy: serde_json::Value =
            serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            policy["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "phantom@example.com"),
            "phantom user must appear in policy after deferred resolution"
        );
    }

    // ── mode-specific tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn username_mode_deactivation_expires_nodes_by_name() {
        // Username mode: nodes are matched by node.user.name, not email.
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "alice".to_string(),
            email: String::new(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::Username,
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let created = create_alice(&svc).await;

        svc.put_user(
            &created.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(false),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .expect("deactivation must succeed in Username mode");

        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_some(),
            "node must be expired by username match in Username mode"
        );
    }

    #[tokio::test]
    async fn username_mode_deletion_deletes_headscale_user_by_name() {
        // Username mode: headscale user found by name, not email.
        let server = FakeHeadscaleServer::default();
        server.state.lock().unwrap().users.push(User {
            id: 1,
            name: "alice".to_string(),
            email: String::new(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::Username,
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let created = create_alice(&svc).await;
        svc.delete_user(&created.id)
            .await
            .expect("delete must succeed in Username mode");

        assert!(
            headscale_state.lock().unwrap().users.is_empty(),
            "headscale user must be deleted by name in Username mode"
        );
    }

    #[tokio::test]
    async fn username_mode_policy_uses_at_suffix_token() {
        // Username mode policy entries use "username@" format.
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::Username,
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let alice = create_alice(&svc).await;
        svc.create_group(GroupBody {
            display_name: Some("eng".to_string()),
            external_id: None,
            members: Some(vec![ScimMember {
                value: alice.id.clone(),
                display: None,
            }]),
        })
        .await
        .unwrap();

        let live: serde_json::Value = serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        assert!(
            live["groups"]["group:eng"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "alice@"),
            "Username mode must write 'username@' token to policy"
        );
    }

    #[tokio::test]
    async fn username_mode_policy_rebuilds_on_rename() {
        // Username mode: renaming a user changes the policy token; reconcile must update it.
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::Username,
            expire_nodes_on_change: false,
        };
        let (svc, mapping, _dir) = make_service_with_config(server, config).await;

        let alice = create_alice(&svc).await;
        mapping
            .lock()
            .await
            .set_group(
                GroupScimId("gid-1".to_string()),
                GroupRecord {
                    name: "eng".to_string(),
                    members: vec![ScimId(alice.id.clone())],
                    external_id: None,
                    updated_at: Default::default(),
                },
            )
            .await
            .unwrap();

        svc.put_user(
            &alice.id,
            UserBody {
                user_name: Some("alice-renamed".to_string()),
                active: Some(true),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let live: serde_json::Value = serde_json::from_str(&policy_store.lock().unwrap()).unwrap();
        let members = live["groups"]["group:eng"].as_array().unwrap();
        assert!(
            members.iter().any(|v| v == "alice-renamed@"),
            "policy must reflect new username after rename"
        );
        assert!(
            !members.iter().any(|v| v == "alice@"),
            "old username token must be gone after rename"
        );
    }

    #[tokio::test]
    async fn external_id_mode_deactivation_expires_nodes_by_provider_id() {
        // ExternalId mode: nodes matched by provider_id = oidc_issuer + "/" + external_id.
        let oidc_issuer = "https://idp.example.com";
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "alice".to_string(),
            email: String::new(),
            provider_id: format!("{}/ext-alice", oidc_issuer),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::ExternalId {
                oidc_issuer: oidc_issuer.to_string(),
            },
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let created = create_alice(&svc).await;

        svc.put_user(
            &created.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(false),
                external_id: Some("ext-alice".to_string()),
                emails: None,
                ..Default::default()
            },
        )
        .await
        .expect("deactivation must succeed in ExternalId mode");

        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_some(),
            "node must be expired by provider_id match in ExternalId mode"
        );
    }

    #[tokio::test]
    async fn external_id_mode_deletion_deletes_headscale_user_by_provider_id() {
        let oidc_issuer = "https://idp.example.com";
        let server = FakeHeadscaleServer::default();
        server.state.lock().unwrap().users.push(User {
            id: 1,
            name: "alice".to_string(),
            email: String::new(),
            provider_id: format!("{}/ext-alice", oidc_issuer),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::ExternalId {
                oidc_issuer: oidc_issuer.to_string(),
            },
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let created = create_alice(&svc).await;
        svc.delete_user(&created.id)
            .await
            .expect("delete must succeed in ExternalId mode");

        assert!(
            headscale_state.lock().unwrap().users.is_empty(),
            "headscale user must be deleted by provider_id in ExternalId mode"
        );
    }

    #[tokio::test]
    async fn external_id_mode_policy_uses_provider_id_token_with_comment() {
        // ExternalId mode: policy token is "oidc_issuer/external_id@" with block comment.
        let oidc_issuer = "https://idp.example.com";
        let server = FakeHeadscaleServer::default();
        let policy_store = Arc::clone(&server.policy);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::ExternalId {
                oidc_issuer: oidc_issuer.to_string(),
            },
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let alice = create_alice(&svc).await;
        svc.create_group(GroupBody {
            display_name: Some("eng".to_string()),
            external_id: None,
            members: Some(vec![ScimMember {
                value: alice.id.clone(),
                display: None,
            }]),
        })
        .await
        .unwrap();

        let raw = policy_store.lock().unwrap().clone();
        // Raw policy must contain the provider_id token and block comment.
        assert!(
            raw.contains("https://idp.example.com/ext-alice@"),
            "ExternalId token must be present in raw policy"
        );
        assert!(
            raw.contains("/* alice@example.com, alice */"),
            "block comment must appear in ExternalId mode policy: {raw}"
        );
        // Parsed as JSONC, the token value is the provider_id string.
        let v: serde_json::Value = jsonc_parser::parse_to_serde_value::<serde_json::Value>(
            &raw,
            &jsonc_parser::ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(
            v["groups"]["group:eng"][0],
            "https://idp.example.com/ext-alice@"
        );
    }

    #[tokio::test]
    async fn external_id_mode_no_email_deactivation_succeeds() {
        // ExternalId mode: user without email can be deactivated (provider_id is used).
        let oidc_issuer = "https://idp.example.com";
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "no-email-user".to_string(),
            email: String::new(),
            provider_id: format!("{}/ext-no-email", oidc_issuer),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::ExternalId {
                oidc_issuer: oidc_issuer.to_string(),
            },
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let (_, created) = svc
            .create_user(UserBody {
                user_name: Some("no-email-user".to_string()),
                emails: None,
                external_id: Some("ext-no-email".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("no-email-user".to_string()),
                    active: Some(false),
                    external_id: Some("ext-no-email".to_string()),
                    emails: None,
                    ..Default::default()
                },
            )
            .await
            .expect("deactivation without email must succeed in ExternalId mode");

        assert!(!result.active);
        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_some(),
            "node must be expired even without email in ExternalId mode"
        );
    }

    #[tokio::test]
    async fn external_id_mode_create_user_without_external_id_is_rejected() {
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::ExternalId {
                oidc_issuer: "https://idp.example.com".to_string(),
            },
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(FakeHeadscaleServer::default(), config).await;

        let result = svc
            .create_user(UserBody {
                user_name: Some("alice".to_string()),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                external_id: None,
                ..Default::default()
            })
            .await;

        assert!(
            result.is_err(),
            "create_user must reject missing externalId in ExternalId mode"
        );
        let err = result.unwrap_err();
        assert_eq!(
            err.status,
            axum::http::StatusCode::BAD_REQUEST,
            "must return HTTP 400"
        );
    }

    #[tokio::test]
    async fn external_id_mode_put_user_without_external_id_is_rejected() {
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::ExternalId {
                oidc_issuer: "https://idp.example.com".to_string(),
            },
            expire_nodes_on_change: false,
        };
        let (svc, _, _dir) = make_service_with_config(FakeHeadscaleServer::default(), config).await;

        // Create succeeds (with externalId).
        let (_, created) = svc
            .create_user(UserBody {
                user_name: Some("alice".to_string()),
                external_id: Some("ext-alice".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        // PUT without externalId must be rejected.
        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("alice".to_string()),
                    active: Some(false),
                    external_id: None,
                    ..Default::default()
                },
            )
            .await;

        assert!(
            result.is_err(),
            "put_user must reject missing externalId in ExternalId mode"
        );
        let err = result.unwrap_err();
        assert_eq!(err.status, 400, "must return HTTP 400");
    }

    #[tokio::test]
    async fn email_mode_no_email_deactivation_logs_warning_and_skips() {
        // Email mode: user without email cannot have nodes expired (no lookup key).
        // This is a known gap — deactivation succeeds but headscale session is NOT killed.
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "no-email-user".to_string(),
            email: String::new(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await; // default = Email mode

        let (_, created) = svc
            .create_user(UserBody {
                user_name: Some("no-email-user".to_string()),
                emails: None,
                ..Default::default()
            })
            .await
            .unwrap();

        let result = svc
            .put_user(
                &created.id,
                UserBody {
                    user_name: Some("no-email-user".to_string()),
                    active: Some(false),
                    emails: None,
                    ..Default::default()
                },
            )
            .await
            .expect("deactivation must succeed even when node expiry is skipped");

        assert!(!result.active, "user must be marked inactive");

        // Node is NOT expired — Email mode cannot find it without an email address.
        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_none(),
            "node must NOT be expired in Email mode when user has no email (expected gap)"
        );
    }

    #[tokio::test]
    async fn expire_nodes_on_change_email_mode_triggers_on_email_change() {
        // expire_nodes_on_change=true: changing email in Email mode expires existing nodes.
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "alice".to_string(),
            email: "alice@example.com".to_string(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::Email,
            expire_nodes_on_change: true,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let created = create_alice(&svc).await;

        // Change email while keeping active=true.
        svc.put_user(
            &created.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(true),
                emails: Some(vec![ScimEmail {
                    value: "alice@new.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .expect("email change must succeed");

        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_some(),
            "node must be expired on email change when expire_nodes_on_change=true"
        );
    }

    #[tokio::test]
    async fn expire_nodes_on_change_does_not_trigger_without_flag() {
        // expire_nodes_on_change=false (default): email change does NOT expire nodes.
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "alice".to_string(),
            email: "alice@example.com".to_string(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let (svc, _, _dir) = make_service(server).await; // default config, expire_nodes_on_change=false

        let created = create_alice(&svc).await;

        svc.put_user(
            &created.id,
            UserBody {
                user_name: Some("alice".to_string()),
                active: Some(true),
                emails: Some(vec![ScimEmail {
                    value: "alice@new.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .expect("email change must succeed");

        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_none(),
            "node must NOT be expired on email change when expire_nodes_on_change=false"
        );
    }

    #[tokio::test]
    async fn expire_nodes_on_change_username_mode_triggers_on_rename() {
        // expire_nodes_on_change=true in Username mode: renaming expires existing nodes.
        let server = FakeHeadscaleServer::default();
        let node_id = server.create_node_for_user(User {
            id: 1,
            name: "alice".to_string(),
            ..Default::default()
        });
        let headscale_state = Arc::clone(&server.state);
        let config = ScimConfig {
            policy_user_key: PolicyUserKey::Username,
            expire_nodes_on_change: true,
        };
        let (svc, _, _dir) = make_service_with_config(server, config).await;

        let created = create_alice(&svc).await;

        svc.put_user(
            &created.id,
            UserBody {
                user_name: Some("alice-renamed".to_string()),
                active: Some(true),
                emails: Some(vec![ScimEmail {
                    value: "alice@example.com".to_string(),
                    primary: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .expect("rename must succeed");

        let state = headscale_state.lock().unwrap();
        let node = state.nodes.iter().find(|n| n.id == node_id).unwrap();
        assert!(
            node.expiry.is_some(),
            "node must be expired on username change when expire_nodes_on_change=true in Username mode"
        );
    }
}
