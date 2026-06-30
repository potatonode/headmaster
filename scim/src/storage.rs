use std::collections::BTreeMap;
use std::path::PathBuf;

use jiff::Timestamp;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::types::{ExternalId, GroupScimId, ScimId};

// ── storage types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UserRecord {
    /// Whether the IdP considers this user active. Inactive users are excluded
    /// from the headscale policy and have their headscale user deleted.
    pub active: bool,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<ExternalId>,
    /// Set by `set_user` on every write; surfaced as `meta.lastModified`.
    #[serde(default = "Timestamp::now")]
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GroupRecord {
    pub name: String,
    /// Ordered SCIM user UUIDs. Emails are resolved at reconcile time via UserRecord.
    /// Inactive users are filtered out during policy reconciliation.
    pub members: Vec<ScimId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<ExternalId>,
    /// Surfaced as `meta.lastModified`. Callers set this; `set_group` preserves it.
    /// POST-based group creation uses epoch so Pocket ID's next sync always
    /// sends a PUT with current membership. Explicit PUTs use `Timestamp::now()`.
    #[serde(default = "Timestamp::now")]
    pub updated_at: Timestamp,
}

// ── Mapping ───────────────────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct MappingFile {
    #[serde(default)]
    users: BTreeMap<ScimId, UserRecord>,
    #[serde(default)]
    groups: BTreeMap<GroupScimId, GroupRecord>,
}

#[derive(Debug, Default)]
pub struct Mapping {
    path: PathBuf,
    data: MappingFile,
}

impl Mapping {
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, std::io::Error> {
        let path = path.into();
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => {
                serde_json::from_str(&raw).map_err(|e| std::io::Error::other(e.to_string()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => MappingFile::default(),
            Err(e) => return Err(e),
        };
        Ok(Self { path, data })
    }

    // ── user records ──────────────────────────────────────────────────────────

    pub fn get_user(&self, id: &ScimId) -> Option<&UserRecord> {
        self.data.users.get(id)
    }

    pub fn get_user_by_name(&self, name: &str) -> Option<(&ScimId, &UserRecord)> {
        self.data.users.iter().find(|(_, r)| r.name == name)
    }

    /// Iterates over all user records.
    pub fn users(&self) -> impl Iterator<Item = (&ScimId, &UserRecord)> {
        self.data.users.iter()
    }

    pub async fn set_user(
        &mut self,
        id: &ScimId,
        mut record: UserRecord,
    ) -> Result<UserRecord, std::io::Error> {
        record.updated_at = Timestamp::now();
        self.data.users.insert(id.clone(), record.clone());
        self.flush().await?;
        Ok(record)
    }

    pub async fn remove_user(&mut self, id: &ScimId) -> Result<(), std::io::Error> {
        let Some(removed) = self.data.users.remove(id) else {
            return Ok(());
        };
        if let Err(e) = self.flush().await {
            self.data.users.insert(id.clone(), removed);
            return Err(e);
        }
        Ok(())
    }

    /// Removes the user record and strips them from all group member lists in a
    /// single flush, eliminating the crash window that would otherwise exist
    /// between two separate flushes.
    pub async fn remove_user_and_from_groups(&mut self, id: &ScimId) -> Result<(), std::io::Error> {
        let removed_user = self.data.users.remove(id);

        let rollback_groups: Vec<(GroupScimId, Vec<ScimId>)> = self
            .data
            .groups
            .iter()
            .filter(|(_, r)| r.members.contains(id))
            .map(|(gid, r)| (gid.clone(), r.members.clone()))
            .collect();

        for record in self.data.groups.values_mut() {
            record.members.retain(|m| m != id);
        }

        if let Err(e) = self.flush().await {
            if let Some(user) = removed_user {
                self.data.users.insert(id.clone(), user);
            }
            for (gid, members) in rollback_groups {
                if let Some(record) = self.data.groups.get_mut(&gid) {
                    record.members = members;
                }
            }
            return Err(e);
        }
        Ok(())
    }

    // ── group records ─────────────────────────────────────────────────────────

    pub fn get_group(&self, id: &GroupScimId) -> Option<&GroupRecord> {
        self.data.groups.get(id)
    }

    pub fn get_group_by_name(&self, name: &str) -> Option<(&GroupScimId, &GroupRecord)> {
        self.data.groups.iter().find(|(_, r)| r.name == name)
    }

    pub fn groups(&self) -> impl Iterator<Item = (&GroupScimId, &GroupRecord)> {
        self.data.groups.iter()
    }

    pub async fn set_group(
        &mut self,
        id: GroupScimId,
        record: GroupRecord,
    ) -> Result<GroupRecord, std::io::Error> {
        self.data.groups.insert(id, record.clone());
        self.flush().await?;
        Ok(record)
    }

    pub async fn remove_group(&mut self, id: &GroupScimId) -> Result<(), std::io::Error> {
        let Some(removed) = self.data.groups.remove(id) else {
            return Ok(());
        };
        if let Err(e) = self.flush().await {
            self.data.groups.insert(id.clone(), removed);
            return Err(e);
        }
        Ok(())
    }

    // ── persistence ───────────────────────────────────────────────────────────

    async fn flush(&self) -> Result<(), std::io::Error> {
        let data = serde_json::to_string_pretty(&self.data)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = self.path.with_extension("tmp");
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(data.as_bytes()).await?;
        file.sync_data().await?;
        drop(file);
        tokio::fs::rename(&tmp, &self.path).await?;
        // fsync the parent directory so the rename's directory entry is durable.
        // Without this a crash after rename but before the OS flushes the directory
        // journal can leave mapping.json pointing to the pre-rename version.
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::File::open(parent).await?.sync_all().await?;
        }
        Ok(())
    }
}

pub type SharedMapping = std::sync::Arc<Mutex<Mapping>>;

pub fn shared(mapping: Mapping) -> SharedMapping {
    std::sync::Arc::new(Mutex::new(mapping))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn tmp_mapping() -> (Mapping, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");
        (Mapping::load(&path).await.unwrap(), dir)
    }

    fn alice_scim_id() -> ScimId {
        ScimId("550e8400-e29b-41d4-a716-446655440001".to_string())
    }

    fn bob_scim_id() -> ScimId {
        ScimId("550e8400-e29b-41d4-a716-446655440002".to_string())
    }

    fn eng_group_id() -> GroupScimId {
        GroupScimId("uuid-1".to_string())
    }

    fn alice_record() -> UserRecord {
        UserRecord {
            active: true,
            name: "alice".to_string(),
            display_name: Some("Alice Smith".to_string()),
            email: Some("alice@example.com".to_string()),
            external_id: Some(ExternalId("ext-alice".to_string())),
            updated_at: Default::default(),
        }
    }

    fn eng_record() -> GroupRecord {
        GroupRecord {
            name: "eng".to_string(),
            members: vec![alice_scim_id()],
            external_id: Some(ExternalId("ext-eng".to_string())),
            updated_at: Default::default(),
        }
    }

    // ── user records ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn user_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");

        let mut m = Mapping::load(&path).await.unwrap();
        m.set_user(&alice_scim_id(), alice_record()).await.unwrap();

        let m2 = Mapping::load(&path).await.unwrap();
        let r = m2
            .get_user(&alice_scim_id())
            .expect("alice must be present");
        assert!(r.active);
        assert_eq!(r.name, "alice");
        assert_eq!(r.display_name.as_deref(), Some("Alice Smith"));
        assert_eq!(r.email.as_deref(), Some("alice@example.com"));
        assert_eq!(
            r.external_id.as_ref().map(ExternalId::as_str),
            Some("ext-alice")
        );
    }

    #[tokio::test]
    async fn remove_user_clears_record() {
        let (mut m, _dir) = tmp_mapping().await;
        m.set_user(&alice_scim_id(), alice_record()).await.unwrap();
        m.remove_user(&alice_scim_id()).await.unwrap();
        assert!(m.get_user(&alice_scim_id()).is_none());
    }

    #[tokio::test]
    async fn user_active_flag_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");

        let mut m = Mapping::load(&path).await.unwrap();
        m.set_user(
            &bob_scim_id(),
            UserRecord {
                active: false,
                name: "bob".to_string(),
                display_name: None,
                email: Some("bob@example.com".to_string()),
                external_id: None,
                updated_at: Default::default(),
            },
        )
        .await
        .unwrap();

        let m2 = Mapping::load(&path).await.unwrap();
        assert!(!m2.get_user(&bob_scim_id()).unwrap().active);
    }

    // ── group records ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn group_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");

        let mut m = Mapping::load(&path).await.unwrap();
        m.set_group(eng_group_id(), eng_record()).await.unwrap();

        let m2 = Mapping::load(&path).await.unwrap();
        let r = m2.get_group(&eng_group_id()).expect("eng must be present");
        assert_eq!(r.name, "eng");
        assert_eq!(r.members, vec![alice_scim_id()]);
        assert_eq!(
            r.external_id.as_ref().map(ExternalId::as_str),
            Some("ext-eng")
        );
    }

    #[tokio::test]
    async fn get_group_by_name() {
        let (mut m, _dir) = tmp_mapping().await;
        m.set_group(eng_group_id(), eng_record()).await.unwrap();
        let (id, r) = m.get_group_by_name("eng").expect("must find eng by name");
        assert_eq!(id, &eng_group_id());
        assert_eq!(r.name, "eng");
    }

    #[tokio::test]
    async fn get_group_by_name_unknown() {
        let (m, _dir) = tmp_mapping().await;
        assert!(m.get_group_by_name("nonexistent").is_none());
    }

    #[tokio::test]
    async fn remove_group_clears_record() {
        let (mut m, _dir) = tmp_mapping().await;
        m.set_group(eng_group_id(), eng_record()).await.unwrap();
        m.remove_group(&eng_group_id()).await.unwrap();
        assert!(m.get_group(&eng_group_id()).is_none());
    }

    #[tokio::test]
    async fn set_group_preserves_caller_updated_at() {
        let (mut m, _dir) = tmp_mapping().await;
        let now = Timestamp::now();
        let record = GroupRecord {
            updated_at: now,
            ..eng_record()
        };
        let stored = m.set_group(eng_group_id(), record).await.unwrap();
        assert_eq!(stored.updated_at, now);
    }

    #[tokio::test]
    async fn set_group_epoch_when_caller_passes_default() {
        let (mut m, _dir) = tmp_mapping().await;
        let stored = m.set_group(eng_group_id(), eng_record()).await.unwrap();
        assert_eq!(stored.updated_at, Default::default());
    }

    #[tokio::test]
    async fn remove_user_rolls_back_on_flush_failure() {
        // Point the mapping at a read-only directory so flush always fails.
        // After the failure the user must still be present in memory (rolled back),
        // not silently deleted, so a subsequent retry can succeed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");
        let mut m = Mapping::load(&path).await.unwrap();
        m.set_user(&alice_scim_id(), alice_record()).await.unwrap();

        // Make the directory read-only so flush fails.
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();

        let err = m.remove_user(&alice_scim_id()).await;

        // Restore permissions before any assertions (so the tempdir cleanup works).
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        assert!(err.is_err(), "remove_user must propagate flush failures");
        assert!(
            m.get_user(&alice_scim_id()).is_some(),
            "user must be rolled back in memory when flush fails"
        );
    }

    #[tokio::test]
    async fn remove_group_rolls_back_on_flush_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");
        let mut m = Mapping::load(&path).await.unwrap();
        m.set_group(eng_group_id(), eng_record()).await.unwrap();

        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();

        let err = m.remove_group(&eng_group_id()).await;

        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        assert!(err.is_err(), "remove_group must propagate flush failures");
        assert!(
            m.get_group(&eng_group_id()).is_some(),
            "group must be rolled back in memory when flush fails"
        );
    }

    #[tokio::test]
    async fn remove_user_and_from_groups_clears_both() {
        let (mut m, _dir) = tmp_mapping().await;
        m.set_user(&alice_scim_id(), alice_record()).await.unwrap();
        m.set_group(eng_group_id(), eng_record()).await.unwrap();

        m.remove_user_and_from_groups(&alice_scim_id())
            .await
            .unwrap();

        assert!(
            m.get_user(&alice_scim_id()).is_none(),
            "user record must be removed"
        );
        assert!(
            m.get_group(&eng_group_id()).unwrap().members.is_empty(),
            "user must be removed from group members"
        );
    }

    #[tokio::test]
    async fn remove_user_and_from_groups_rolls_back_on_flush_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");
        let mut m = Mapping::load(&path).await.unwrap();
        m.set_user(&alice_scim_id(), alice_record()).await.unwrap();
        m.set_group(eng_group_id(), eng_record()).await.unwrap();

        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();

        let err = m.remove_user_and_from_groups(&alice_scim_id()).await;

        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        assert!(err.is_err(), "must propagate flush failures");
        assert!(
            m.get_user(&alice_scim_id()).is_some(),
            "user must be rolled back in memory when flush fails"
        );
        assert_eq!(
            m.get_group(&eng_group_id()).unwrap().members,
            vec![alice_scim_id()],
            "group members must be rolled back in memory when flush fails"
        );
    }

    #[tokio::test]
    async fn group_rename_via_set_group() {
        let (mut m, _dir) = tmp_mapping().await;
        m.set_group(eng_group_id(), eng_record()).await.unwrap();

        let renamed = GroupRecord {
            name: "engineering".to_string(),
            ..eng_record()
        };
        m.set_group(eng_group_id(), renamed).await.unwrap();

        assert_eq!(m.get_group(&eng_group_id()).unwrap().name, "engineering");
        assert!(m.get_group_by_name("eng").is_none());
        assert!(m.get_group_by_name("engineering").is_some());
    }
}
