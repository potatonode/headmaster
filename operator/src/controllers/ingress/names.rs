use sha2::Digest;

pub(super) struct ProxyNames {
    pub(super) proxy_base: String,
    pub(super) proxy_name: String,
    pub(super) wg_service_name: String,
    pub(super) config_secret_name: String,
    pub(super) state_secret_name: String,
    pub(super) serve_configmap_name: String,
}

impl ProxyNames {
    pub(super) fn new(ingress_ns: &str, ingress_name: &str) -> Self {
        // Use a null-byte separator for the hash input so the encoding is injective:
        // namespace "a-b" + name "c" and namespace "a" + name "b-c" both display
        // as "a-b-c" with a dash separator, but their null-separated hashes differ.
        // The 8-char hash suffix is always included so every (ns, name) pair maps
        // to a unique proxy_base regardless of length.
        let hash_input = format!("{ingress_ns}\x00{ingress_name}");
        let hash = &hex::encode(sha2::Sha256::digest(hash_input.as_bytes()))[..8];
        let display = format!("{ingress_ns}-{ingress_name}");
        let prefix = if display.len() <= 40 {
            display
        } else {
            // floor_char_boundary avoids a panic when a multi-byte char straddles position 40.
            let cut = display.floor_char_boundary(40);
            display[..cut].to_string()
        };
        let proxy_base = format!("{prefix}-{hash}");
        Self {
            proxy_name: format!("proxy-{proxy_base}"),
            wg_service_name: format!("proxy-wg-{proxy_base}"),
            config_secret_name: format!("proxy-authkey-{proxy_base}"),
            state_secret_name: format!("proxy-state-{proxy_base}"),
            serve_configmap_name: format!("proxy-serve-{proxy_base}"),
            proxy_base,
        }
    }
}

/// Returns the name of the proxy StatefulSet for the given Ingress.
/// Exposed for use in integration tests.
pub fn proxy_sts_name(ingress_ns: &str, ingress_name: &str) -> String {
    ProxyNames::new(ingress_ns, ingress_name).proxy_name
}

/// Returns the name of the proxy state Secret for the given Ingress.
/// Exposed for use in integration tests.
pub fn proxy_state_secret_name(ingress_ns: &str, ingress_name: &str) -> String {
    ProxyNames::new(ingress_ns, ingress_name).state_secret_name
}

/// Returns the operator-assigned tag for an Ingress with access grants.
pub fn ingress_auto_tag(ingress_ns: &str, ingress_name: &str) -> String {
    format!(
        "tag:hm-{}",
        ProxyNames::new(ingress_ns, ingress_name).proxy_base
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_names_short() {
        let base = ProxyNames::new("default", "my-app").proxy_base;
        // Always prefix-hash; short display names appear verbatim before the hash.
        assert!(
            base.starts_with("default-my-app-"),
            "short name must appear as readable prefix: {base}"
        );
        assert_eq!(
            base.len(),
            "default-my-app-".len() + 8,
            "hash suffix must be exactly 8 hex chars"
        );
    }

    #[test]
    fn proxy_names_collision_free() {
        // "a-b"/"c" and "a"/"b-c" both display as "a-b-c" with a dash separator.
        // The null-byte-separated hash must distinguish them.
        let base1 = ProxyNames::new("a-b", "c").proxy_base;
        let base2 = ProxyNames::new("a", "b-c").proxy_base;
        assert_ne!(
            base1, base2,
            "colliding display names must produce distinct proxy_base"
        );
    }

    #[test]
    fn proxy_names_over_limit() {
        let ns = "a".repeat(30);
        let name = "b".repeat(30);
        // prefix(≤40) + "-" + 8-char hash = ≤49
        assert_eq!(ProxyNames::new(&ns, &name).proxy_base.len(), 49);
    }

    #[test]
    fn proxy_names_over_limit_is_deterministic() {
        let ns = "a".repeat(30);
        let name = "b".repeat(30);
        assert_eq!(
            ProxyNames::new(&ns, &name).proxy_base,
            ProxyNames::new(&ns, &name).proxy_base,
        );
    }

    #[test]
    fn ingress_auto_tag_format() {
        let tag = ingress_auto_tag("default", "my-app");
        assert!(
            tag.starts_with("tag:hm-default-my-app-"),
            "tag must start with readable prefix: {tag}"
        );
        assert_eq!(
            tag.len(),
            "tag:hm-default-my-app-".len() + 8,
            "hash suffix must be exactly 8 hex chars"
        );
    }

    #[test]
    fn ingress_auto_tag_collision_free() {
        let tag1 = ingress_auto_tag("a-b", "c");
        let tag2 = ingress_auto_tag("a", "b-c");
        assert_ne!(
            tag1, tag2,
            "colliding display names must produce distinct tags"
        );
    }

    #[test]
    fn ingress_auto_tag_over_limit() {
        let ns = "a".repeat(30);
        let name = "b".repeat(30);
        // "tag:hm-" (7) + prefix(≤40) + "-" + 8-char hash = ≤56
        assert_eq!(ingress_auto_tag(&ns, &name).len(), 7 + 40 + 1 + 8);
    }
}
