use std::collections::HashSet;

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use jsonc_parser::{JsonValue, parse_to_value};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("policy parse error: {0}")]
pub struct PolicyParseError(String);

pub struct PolicyMember {
    pub token: String,
    pub comment: Option<String>,
}

pub struct PolicyEditor {
    root: CstRootNode,
}

impl PolicyEditor {
    pub fn parse(s: &str) -> Result<Self, PolicyParseError> {
        let text = if s.trim().is_empty() { "{}" } else { s };
        let root = CstRootNode::parse(text, &ParseOptions::default())
            .map_err(|e| PolicyParseError(e.to_string()))?;
        Ok(Self { root })
    }

    /// Replaces the entire `groups` key in the policy with entries built from
    /// `groups`. Preserves all other top-level keys. When `groups` is empty the
    /// key is removed rather than written as `{}`.
    pub fn set_groups(&mut self, groups: &[(String, Vec<PolicyMember>)]) {
        let root_obj = self.root.object_value_or_set();
        if let Some(prop) = root_obj.get("groups") {
            prop.remove();
        }
        if !groups.is_empty() {
            let groups_obj = root_obj.object_value_or_set("groups");
            for (name, members) in groups {
                let key = format!("group:{name}");
                let prop = groups_obj.append(&key, CstInputValue::Array(vec![]));
                if let Some(arr) = prop.array_value() {
                    for member in members {
                        let node = arr.append(CstInputValue::String(member.token.clone()));
                        if let Some(ref comment) = member.comment
                            && let Some(lit) = node.as_string_lit()
                        {
                            // Block comment (/* */) required — a line comment (//)
                            // would consume the trailing comma, breaking JSON structure.
                            // Sanitize */ to prevent early comment close.
                            let safe = comment.replace("*/", "* /");
                            let json_token = serde_json::to_string(&member.token)
                                .expect("string serialization is infallible");
                            lit.set_raw_value(format!("{json_token} /* {safe} */"));
                        }
                    }
                }
            }
        }
    }

    /// Removes `groups` from `self` and splices in the raw CST bytes of
    /// `other`'s groups value, preserving block comments written by SCIM for
    /// ExternalId mode. When `other` has no groups, self's groups are removed
    /// and nothing is added.
    pub fn copy_groups_from(&mut self, other: &Self) {
        let self_obj = self.root.object_value_or_set();
        if let Some(prop) = self_obj.get("groups") {
            prop.remove();
        }

        let Some(other_obj) = other.root.object_value() else {
            return;
        };
        let Some(other_groups_prop) = other_obj.get("groups") else {
            return;
        };
        let Some(groups_value_node) = other_groups_prop.value() else {
            return;
        };

        let groups_raw = groups_value_node.to_string();
        let new_prop = self_obj.append("groups", CstInputValue::String(String::new()));
        if let Some(lit) = new_prop.value().and_then(|v| v.as_string_lit()) {
            lit.set_raw_value(groups_raw);
        }
    }

    /// Returns the set of group names (e.g. `"group:eng"`) present as keys in
    /// the `groups` section of the current policy.
    pub fn known_groups(&self) -> HashSet<String> {
        let Some(root_obj) = self.root.object_value() else {
            return HashSet::new();
        };
        let Some(groups_obj) = root_obj.object_value("groups") else {
            return HashSet::new();
        };
        groups_obj
            .properties()
            .into_iter()
            .filter_map(|prop| prop.name()?.decoded_value().ok())
            .collect()
    }

    /// Ensures `tag` is present in `tagOwners` with at least `owners` as
    /// members. Any existing owners for `tag` are preserved (union semantics).
    /// If `tag` is absent, it is created. If already present with all owners,
    /// the property is removed and re-added with the merged array.
    pub fn set_tag_owner(&mut self, tag: &str, owners: &[&str]) {
        let existing_owners: Vec<String> = self
            .root
            .object_value()
            .and_then(|root_obj| root_obj.object_value("tagOwners"))
            .and_then(|tag_owners_obj| tag_owners_obj.array_value(tag))
            .map(|arr| {
                arr.elements()
                    .into_iter()
                    .filter_map(|node| node.as_string_lit()?.decoded_value().ok())
                    .collect()
            })
            .unwrap_or_default();

        let mut all_owners: Vec<String> = existing_owners;
        for &owner in owners {
            if !all_owners.iter().any(|o| o == owner) {
                all_owners.push(owner.to_string());
            }
        }

        let root_obj = self.root.object_value_or_set();
        let tag_owners_obj = root_obj.object_value_or_set("tagOwners");
        if let Some(prop) = tag_owners_obj.get(tag) {
            prop.remove();
        }
        let owners_arr = CstInputValue::Array(
            all_owners
                .iter()
                .map(|o| CstInputValue::String(o.clone()))
                .collect(),
        );
        tag_owners_obj.append(tag, owners_arr);
    }

    /// Appends `grants` to the `grants` array, preserving any grants the user
    /// already declared and all HuJSON comments elsewhere in the policy.
    pub fn append_grants(&mut self, grants: &[CstInputValue]) {
        if grants.is_empty() {
            return;
        }
        let root_obj = self.root.object_value_or_set();
        let grants_arr = root_obj.array_value_or_set("grants");
        for grant in grants {
            grants_arr.append(grant.clone());
        }
    }

    /// Removes references to `removed_groups` from every `src` and `dst` array
    /// in `grants`. A grant is removed entirely only when `src` or `dst` becomes
    /// empty after pruning — preserving grants that still have other members.
    pub fn prune_grants_for_removed_groups(&mut self, removed_groups: &HashSet<String>) {
        let Some(root_obj) = self.root.object_value() else {
            return;
        };
        let Some(grants_arr) = root_obj.array_value("grants") else {
            return;
        };

        let grants = grants_arr.elements();

        for grant in &grants {
            let Some(grant_obj) = grant.as_object() else {
                continue;
            };
            for field in ["src", "dst"] {
                let Some(arr) = grant_obj.array_value(field) else {
                    continue;
                };
                let stale: Vec<_> = arr
                    .elements()
                    .into_iter()
                    .filter(|node| {
                        node.as_string_lit()
                            .and_then(|lit| lit.decoded_value().ok())
                            .map(|s| removed_groups.contains(&s))
                            .unwrap_or(false)
                    })
                    .collect();
                for node in stale {
                    node.remove();
                }
            }
        }

        // After pruning members, remove grants whose src or dst is now empty.
        let empty_grants: Vec<_> = grants
            .iter()
            .filter(|grant| {
                let Some(grant_obj) = grant.as_object() else {
                    return false;
                };
                ["src", "dst"].iter().any(|field| {
                    grant_obj
                        .array_value(field)
                        .map(|a| a.elements().is_empty())
                        .unwrap_or(false)
                })
            })
            .cloned()
            .collect();
        for grant in empty_grants.into_iter().rev() {
            grant.remove();
        }
    }
}

impl std::fmt::Display for PolicyEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.root.to_string())
    }
}

impl Clone for PolicyEditor {
    fn clone(&self) -> Self {
        // CstRootNode is Rc-based; derive(Clone) would be a shallow clone that
        // shares the tree. Round-trip through string for a true deep copy.
        Self::parse(&self.to_string()).expect("serialized policy must re-parse")
    }
}

impl PartialEq for PolicyEditor {
    fn eq(&self, other: &Self) -> bool {
        policies_are_semantically_equal(&self.to_string(), &other.to_string())
    }
}

/// Returns `true` when both policy strings represent the same headscale policy.
/// Parses both sides as HuJSON (a superset of JSON that allows comments and
/// trailing commas), then compares the parsed value trees. Falls back to a
/// trimmed-string comparison when either side cannot be parsed.
///
/// Object comparison is order-independent (backed by HashMap equality).
pub fn policies_are_semantically_equal(a: &str, b: &str) -> bool {
    let parsed_a = parse_to_value(a, &ParseOptions::default()).ok().flatten();
    let parsed_b = parse_to_value(b, &ParseOptions::default()).ok().flatten();
    match (parsed_a, parsed_b) {
        (Some(parsed_a), Some(parsed_b)) => json_values_equal(parsed_a, &parsed_b),
        _ => a.trim() == b.trim(),
    }
}

fn json_values_equal<'a, 'b>(a: JsonValue<'a>, b: &JsonValue<'b>) -> bool {
    match (a, b) {
        (JsonValue::Null, JsonValue::Null) => true,
        (JsonValue::Boolean(a), JsonValue::Boolean(b)) => a == *b,
        (JsonValue::Number(a), JsonValue::Number(b)) => a == *b,
        (JsonValue::String(a), JsonValue::String(b)) => a.as_ref() == b.as_ref(),
        (JsonValue::Array(a), JsonValue::Array(b)) => {
            a.len() == b.len()
                && (0..a.len())
                    .zip(0..b.len())
                    .all(|(ia, ib)| match (a.get(ia), b.get(ib)) {
                        (Some(va), Some(vb)) => json_values_equal(va.clone(), vb),
                        _ => false,
                    })
        }
        (JsonValue::Object(a_obj), JsonValue::Object(b_obj)) => {
            if a_obj.len() != b_obj.len() {
                return false;
            }
            for (key, val_a) in a_obj {
                let Some(val_b) = b_obj.get(&key) else {
                    return false;
                };
                if !json_values_equal(val_a, val_b) {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonc_parser::ParseOptions;

    // ── helpers ───────────────────────────────────────────────────────────────

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

    // ── PolicyEditor::parse ───────────────────────────────────────────────────

    #[test]
    fn parse_empty_string_succeeds() {
        let editor = PolicyEditor::parse("").unwrap();
        assert_eq!(editor.to_string(), "{}");
    }

    #[test]
    fn parse_whitespace_only_succeeds() {
        let editor = PolicyEditor::parse("   \n  ").unwrap();
        assert_eq!(editor.to_string(), "{}");
    }

    #[test]
    fn parse_invalid_json_fails() {
        assert!(PolicyEditor::parse("not json :::").is_err());
    }

    #[test]
    fn parse_hujson_with_comments_succeeds() {
        let s = r#"{ // engineering team
            "groups": { "group:eng": ["alice@example.com"] }
        }"#;
        let editor = PolicyEditor::parse(s).unwrap();
        assert!(editor.to_string().contains("// engineering team"));
    }

    // ── set_groups ────────────────────────────────────────────────────────────

    #[test]
    fn set_groups_builds_fresh_on_empty_policy() {
        let mut editor = PolicyEditor::parse("").unwrap();
        editor.set_groups(&[("eng".to_string(), vec![email("alice@example.com")])]);
        let v = parse_json(&editor.to_string());
        assert_eq!(v["groups"]["group:eng"][0], "alice@example.com");
    }

    #[test]
    fn set_groups_preserves_other_keys() {
        let policy = r#"{"acls": [{"action": "accept"}], "groups": {"group:old": []}}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.set_groups(&[("eng".to_string(), vec![email("alice@example.com")])]);
        let v = parse_json(&editor.to_string());
        assert!(v["acls"].is_array(), "acls must be preserved");
        assert!(v["groups"]["group:old"].is_null(), "old group must be gone");
        assert_eq!(v["groups"]["group:eng"][0], "alice@example.com");
    }

    #[test]
    fn set_groups_with_empty_groups_removes_key() {
        let policy = r#"{"groups": {"group:eng": ["alice@example.com"]}}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.set_groups(&[]);
        let v = parse_json(&editor.to_string());
        assert!(
            v["groups"].is_null(),
            "groups key must be absent when list is empty"
        );
    }

    #[test]
    fn set_groups_empty_policy_empty_groups_stays_empty() {
        let mut editor = PolicyEditor::parse("").unwrap();
        editor.set_groups(&[]);
        let v = parse_json(&editor.to_string());
        assert!(
            v["groups"].is_null(),
            "groups key must not appear when there are no groups"
        );
    }

    #[test]
    fn set_groups_external_id_token_with_block_comment() {
        let mut editor = PolicyEditor::parse("").unwrap();
        editor.set_groups(&[(
            "eng".to_string(),
            vec![ext_id(
                "https://idp.example.com/uuid-1@",
                "alice@example.com, alice",
            )],
        )]);
        let result = editor.to_string();
        assert!(
            result.contains("/* alice@example.com, alice */"),
            "block comment must appear in raw policy output: {result}"
        );
        let v = parse_hujson(&result);
        assert_eq!(
            v["groups"]["group:eng"][0],
            "https://idp.example.com/uuid-1@"
        );
    }

    #[test]
    fn set_groups_username_token_no_comment() {
        let mut editor = PolicyEditor::parse("").unwrap();
        editor.set_groups(&[("eng".to_string(), vec![email("alice@")])]);
        let result = editor.to_string();
        let v = parse_json(&result);
        assert_eq!(v["groups"]["group:eng"][0], "alice@");
        assert!(!result.contains("/*"), "no block comment in Username mode");
    }

    #[test]
    fn set_groups_multiple_members_mixed_comments() {
        let mut editor = PolicyEditor::parse("").unwrap();
        editor.set_groups(&[(
            "eng".to_string(),
            vec![
                ext_id("https://idp/uuid@", "bob@example.com, bob"),
                email("alice@example.com"),
            ],
        )]);
        let result = editor.to_string();
        assert!(result.contains("/* bob@example.com, bob */"));
        let v = parse_hujson(&result);
        let arr = v["groups"]["group:eng"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "https://idp/uuid@");
        assert_eq!(arr[1], "alice@example.com");
    }

    #[test]
    fn set_groups_token_with_quotes_and_backslashes() {
        let mut editor = PolicyEditor::parse("").unwrap();
        editor.set_groups(&[(
            "eng".to_string(),
            vec![ext_id(r#"https://idp/user"name\path@"#, "display name")],
        )]);
        let result = editor.to_string();
        let v = parse_hujson(&result);
        assert_eq!(
            v["groups"]["group:eng"][0],
            r#"https://idp/user"name\path@"#
        );
    }

    #[test]
    fn set_groups_comment_with_close_sequence_is_sanitized() {
        let mut editor = PolicyEditor::parse("").unwrap();
        editor.set_groups(&[(
            "eng".to_string(),
            vec![ext_id("https://idp/uuid@", "C*/O, alice*/evil")],
        )]);
        let result = editor.to_string();
        let comment_body = result
            .split("/* ")
            .nth(1)
            .and_then(|s| s.split(" */").next())
            .unwrap_or("");
        assert!(
            !comment_body.contains("*/"),
            "sanitized comment body must not contain */: {result}"
        );
        let v = parse_hujson(&result);
        assert_eq!(v["groups"]["group:eng"][0], "https://idp/uuid@");
    }

    // ── copy_groups_from ──────────────────────────────────────────────────────

    #[test]
    fn copy_groups_from_copies_groups() {
        let desired = r#"{"acls":[{"action":"accept"}]}"#;
        let current =
            r#"{"acls":[{"action":"drop"}],"groups":{"group:eng":["alice@example.com"]}}"#;
        let mut editor = PolicyEditor::parse(desired).unwrap();
        let current_editor = PolicyEditor::parse(current).unwrap();
        editor.copy_groups_from(&current_editor);
        let v = parse_json(&editor.to_string());
        assert_eq!(v["groups"]["group:eng"][0], "alice@example.com");
        assert_eq!(v["acls"][0]["action"], "accept");
    }

    #[test]
    fn copy_groups_from_removes_groups_when_other_has_none() {
        let desired = r#"{"acls":[],"groups":{"group:old":["alice@example.com"]}}"#;
        let current = r#"{"acls":[]}"#;
        let mut editor = PolicyEditor::parse(desired).unwrap();
        let current_editor = PolicyEditor::parse(current).unwrap();
        editor.copy_groups_from(&current_editor);
        let v = parse_json(&editor.to_string());
        assert!(v["groups"].is_null());
    }

    #[test]
    fn copy_groups_from_preserves_hujson_comments_in_self() {
        let desired = "{\n  // allow all\n  \"acls\": [{\"action\": \"accept\"}]\n}";
        let current = r#"{"groups":{"group:eng":["alice@example.com"]}}"#;
        let mut editor = PolicyEditor::parse(desired).unwrap();
        let current_editor = PolicyEditor::parse(current).unwrap();
        editor.copy_groups_from(&current_editor);
        let result = editor.to_string();
        assert!(
            result.contains("// allow all"),
            "HuJSON comment in self must survive: {result}"
        );
        let v = parse_hujson(&result);
        assert_eq!(v["groups"]["group:eng"][0], "alice@example.com");
    }

    #[test]
    fn copy_groups_from_preserves_external_id_block_comments() {
        let desired = r#"{"acls":[{"action":"accept"}]}"#;
        let current = r#"{"groups":{"group:eng":["https://idp.example.com/uuid@" /* alice@example.com, alice */]}}"#;
        let mut editor = PolicyEditor::parse(desired).unwrap();
        let current_editor = PolicyEditor::parse(current).unwrap();
        editor.copy_groups_from(&current_editor);
        let result = editor.to_string();
        assert!(
            result.contains("/* alice@example.com, alice */"),
            "ExternalId block comment must survive: {result}"
        );
        let v = parse_hujson(&result);
        assert_eq!(v["groups"]["group:eng"][0], "https://idp.example.com/uuid@");
    }

    #[test]
    fn copy_groups_from_replaces_existing_groups() {
        let desired = r#"{"groups":{"group:old":["carol@example.com"]},"acls":[]}"#;
        let current = r#"{"groups":{"group:eng":["alice@example.com"]}}"#;
        let mut editor = PolicyEditor::parse(desired).unwrap();
        let current_editor = PolicyEditor::parse(current).unwrap();
        editor.copy_groups_from(&current_editor);
        let v = parse_json(&editor.to_string());
        assert!(v["groups"]["group:old"].is_null());
        assert_eq!(v["groups"]["group:eng"][0], "alice@example.com");
    }

    #[test]
    fn copy_groups_from_empty_current_removes_groups() {
        let desired = r#"{"acls":[],"groups":{"group:old":[]}}"#;
        let current_editor = PolicyEditor::parse("").unwrap();
        let mut editor = PolicyEditor::parse(desired).unwrap();
        editor.copy_groups_from(&current_editor);
        let v = parse_json(&editor.to_string());
        assert!(v["groups"].is_null());
        assert!(v["acls"].is_array());
    }

    // ── set_tag_owner ─────────────────────────────────────────────────────────

    #[test]
    fn set_tag_owner_adds_entry_when_absent() {
        let mut editor = PolicyEditor::parse(r#"{"acls":[]}"#).unwrap();
        editor.set_tag_owner("tag:headmaster", &["autogroup:admin"]);
        let v = parse_json(&editor.to_string());
        assert_eq!(v["tagOwners"]["tag:headmaster"][0], "autogroup:admin");
    }

    #[test]
    fn set_tag_owner_merges_into_existing_entry() {
        let policy = r#"{"tagOwners":{"tag:server":["alice@example.com"]}}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.set_tag_owner("tag:server", &["bob@example.com"]);
        let v = parse_json(&editor.to_string());
        let owners = v["tagOwners"]["tag:server"].as_array().unwrap();
        assert!(owners.contains(&serde_json::json!("alice@example.com")));
        assert!(owners.contains(&serde_json::json!("bob@example.com")));
    }

    #[test]
    fn set_tag_owner_idempotent_when_owner_already_present() {
        let policy = r#"{"tagOwners":{"tag:headmaster":["autogroup:admin"]}}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.set_tag_owner("tag:headmaster", &["autogroup:admin"]);
        let result = editor.to_string();
        assert!(
            policies_are_semantically_equal(policy, &result),
            "set_tag_owner must be semantically idempotent: {result}"
        );
    }

    #[test]
    fn set_tag_owner_preserves_existing_other_tags() {
        let policy = r#"{"tagOwners":{"tag:server":["alice@example.com"]}}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.set_tag_owner("tag:headmaster", &["autogroup:admin"]);
        let v = parse_json(&editor.to_string());
        assert_eq!(v["tagOwners"]["tag:server"][0], "alice@example.com");
        assert_eq!(v["tagOwners"]["tag:headmaster"][0], "autogroup:admin");
    }

    // ── append_grants ─────────────────────────────────────────────────────────

    #[test]
    fn append_grants_noop_when_empty() {
        let policy = r#"{"acls":[{"action":"accept"}]}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.append_grants(&[]);
        let result = editor.to_string();
        let v = parse_json(&result);
        assert!(v["grants"].is_null(), "no grants must be added");
        assert_eq!(v["acls"][0]["action"], "accept");
    }

    #[test]
    fn append_grants_adds_plain_grant() {
        let policy = r#"{"acls":[{"action":"accept"}]}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.append_grants(&[CstInputValue::Object(vec![
            (
                "src".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("group:eng".to_string())]),
            ),
            (
                "dst".to_string(),
                CstInputValue::Array(vec![CstInputValue::String(
                    "tag:hm-default-myapp".to_string(),
                )]),
            ),
            (
                "ip".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("*:*".to_string())]),
            ),
        ])]);
        let v = parse_json(&editor.to_string());
        assert_eq!(v["grants"][0]["src"][0], "group:eng");
        assert_eq!(v["grants"][0]["dst"][0], "tag:hm-default-myapp");
        assert_eq!(v["grants"][0]["ip"][0], "*:*");
        assert_eq!(v["acls"][0]["action"], "accept", "acls must be preserved");
    }

    #[test]
    fn append_grants_preserves_user_grants() {
        let policy =
            r#"{"grants":[{"src":["group:admins"],"dst":["tag:my-server"],"ip":["*:*"]}]}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.append_grants(&[CstInputValue::Object(vec![
            (
                "src".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("group:eng".to_string())]),
            ),
            (
                "dst".to_string(),
                CstInputValue::Array(vec![CstInputValue::String(
                    "tag:hm-default-myapp".to_string(),
                )]),
            ),
            (
                "ip".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("*:*".to_string())]),
            ),
        ])]);
        let v = parse_json(&editor.to_string());
        let grants = v["grants"].as_array().unwrap();
        assert_eq!(grants.len(), 2);
        assert!(
            grants.iter().any(|g| g["dst"][0] == "tag:my-server"),
            "user grant preserved"
        );
        assert!(
            grants.iter().any(|g| g["dst"][0] == "tag:hm-default-myapp"),
            "operator grant appended"
        );
    }

    #[test]
    fn append_grants_preserves_hujson_comments() {
        let policy = "{\n  // allow all\n  \"acls\": [{\"action\": \"accept\"}]\n}";
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.append_grants(&[CstInputValue::Object(vec![
            (
                "src".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("group:eng".to_string())]),
            ),
            (
                "dst".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("tag:myapp".to_string())]),
            ),
            (
                "ip".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("*:*".to_string())]),
            ),
        ])]);
        let result = editor.to_string();
        assert!(
            result.contains("// allow all"),
            "comment must survive: {result}"
        );
        let v = parse_hujson(&result);
        assert_eq!(v["grants"][0]["src"][0], "group:eng");
    }

    #[test]
    fn append_grants_into_comment_only_policy() {
        let policy = "/* TODO: fill this in */";
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.set_tag_owner("tag:hm-default-myapp-a1b2c3d4", &["tag:headmaster"]);
        editor.append_grants(&[CstInputValue::Object(vec![
            (
                "src".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("group:eng".to_string())]),
            ),
            (
                "dst".to_string(),
                CstInputValue::Array(vec![CstInputValue::String(
                    "tag:hm-default-myapp-a1b2c3d4".to_string(),
                )]),
            ),
            (
                "ip".to_string(),
                CstInputValue::Array(vec![CstInputValue::String("*:*".to_string())]),
            ),
        ])]);
        let result = editor.to_string();
        assert!(
            result.contains("TODO: fill this in"),
            "comment must survive: {result}"
        );
        let v = parse_hujson(&result);
        assert_eq!(v["grants"][0]["src"][0], "group:eng");
        assert_eq!(
            v["tagOwners"]["tag:hm-default-myapp-a1b2c3d4"][0],
            "tag:headmaster"
        );
    }

    // ── prune_grants_for_removed_groups ──────────────────────────────────────

    fn removed(groups: &[&str]) -> HashSet<String> {
        groups.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prune_grants_removes_grant_when_src_becomes_empty() {
        let policy = r#"{
            "grants": [
                {"src": ["group:eng"], "dst": ["tag:app"], "ip": ["*:*"]},
                {"src": ["group:ops"], "dst": ["tag:db"], "ip": ["*:*"]}
            ]
        }"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.prune_grants_for_removed_groups(&removed(&["group:eng"]));
        let v = parse_hujson(&editor.to_string());
        let grants = v["grants"].as_array().unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0]["dst"][0], "tag:db");
    }

    #[test]
    fn prune_grants_prunes_member_from_multi_src_keeps_grant() {
        let policy = r#"{
            "grants": [
                {"src": ["group:eng", "group:ops"], "dst": ["tag:shared"], "ip": ["*:*"]}
            ]
        }"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.prune_grants_for_removed_groups(&removed(&["group:eng"]));
        let v = parse_hujson(&editor.to_string());
        let grants = v["grants"].as_array().unwrap();
        assert_eq!(
            grants.len(),
            1,
            "grant must survive — group:ops still in src"
        );
        let src = grants[0]["src"].as_array().unwrap();
        assert_eq!(src.len(), 1);
        assert_eq!(src[0], "group:ops");
    }

    #[test]
    fn prune_grants_noop_when_group_absent() {
        let policy = r#"{"grants":[{"src":["group:eng"],"dst":["tag:app"],"ip":["*:*"]}]}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.prune_grants_for_removed_groups(&removed(&["group:ops"]));
        let v = parse_json(&editor.to_string());
        assert_eq!(v["grants"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn prune_grants_removes_grant_when_dst_becomes_empty() {
        let policy = r#"{"grants":[{"src":["*"],"dst":["group:eng"],"ip":["*:*"]}]}"#;
        let mut editor = PolicyEditor::parse(policy).unwrap();
        editor.prune_grants_for_removed_groups(&removed(&["group:eng"]));
        let v = parse_json(&editor.to_string());
        assert!(v["grants"].as_array().unwrap().is_empty());
    }

    // ── known_groups ──────────────────────────────────────────────────────────

    #[test]
    fn known_groups_finds_group_keys() {
        let policy = r#"{"groups":{"group:eng":["alice@example.com"],"group:ops":[]}}"#;
        let editor = PolicyEditor::parse(policy).unwrap();
        let groups = editor.known_groups();
        assert!(groups.contains("group:eng"));
        assert!(groups.contains("group:ops"));
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn known_groups_empty_on_no_groups_section() {
        let editor = PolicyEditor::parse(r#"{"acls":[]}"#).unwrap();
        assert!(editor.known_groups().is_empty());
    }

    #[test]
    fn known_groups_empty_on_empty_policy() {
        let editor = PolicyEditor::parse("").unwrap();
        assert!(editor.known_groups().is_empty());
    }

    // ── policies_are_semantically_equal ───────────────────────────────────────

    #[test]
    fn policies_equal_hujson_with_comments_equals_plain_json() {
        let hujson = r#"{
            // engineering team
            "groups": { "group:eng": ["alice@example.com"] }
        }"#;
        let plain = r#"{"groups":{"group:eng":["alice@example.com"]}}"#;
        assert!(
            policies_are_semantically_equal(hujson, plain),
            "HuJSON with comments must compare equal to semantically equivalent plain JSON"
        );
    }

    #[test]
    fn policies_not_equal_when_values_differ() {
        let a = r#"{"groups":{"group:eng":["alice@example.com"]}}"#;
        let b = r#"{"groups":{"group:ops":["bob@example.com"]}}"#;
        assert!(!policies_are_semantically_equal(a, b));
    }

    #[test]
    fn policies_equal_empty_strings() {
        assert!(policies_are_semantically_equal("", ""));
    }

    #[test]
    fn policies_equal_falls_back_to_string_comparison_for_unparseable() {
        let garbage = "not json or hujson :::";
        assert!(policies_are_semantically_equal(garbage, garbage));
        assert!(!policies_are_semantically_equal(garbage, "{}"));
    }

    #[test]
    fn policies_not_equal_empty_vs_unparseable() {
        // parse_to_value("") returns Ok(None); parse_to_value(":::") returns Err.
        // Both collapse to None via .ok().flatten(), so the (None, None) arm must
        // not short-circuit to true — it must fall back to string comparison.
        assert!(!policies_are_semantically_equal("", "not json :::"));
    }

    #[test]
    fn policies_equal_object_order_independent() {
        let a = r#"{"acls":[],"groups":{}}"#;
        let b = r#"{"groups":{},"acls":[]}"#;
        assert!(policies_are_semantically_equal(a, b));
    }
}
