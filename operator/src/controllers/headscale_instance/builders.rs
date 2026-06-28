use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource,
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PodSecurityContext, PodSpec, PodTemplateSpec,
    Probe, ResourceRequirements, SeccompProfile, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi_ext::{
    ConfigMapExt, ConfigMapVolumeSourceExt, ContainerExt, ContainerPortExt, PodSpecExt,
    PodTemplateSpecExt, ProbeExt, StatefulSetExt, VolumeExt, VolumeMountExt,
};
use sha2::{Digest, Sha256};

use super::{PORT_GRPC, PORT_HTTP, PORT_METRICS};
use crate::types::StorageSpec;

pub(super) fn build_configmap(
    name: &str,
    server_url: &str,
    dns_base_domain: &str,
    extra_config: &BTreeMap<String, serde_json::Value>,
) -> Result<(ConfigMap, String), serde_yaml::Error> {
    let config_yaml = build_config(server_url, dns_base_domain, extra_config.clone())?;
    let hash = hex::encode(Sha256::digest(config_yaml.as_bytes()));
    let cm = ConfigMap::new(name).data([("config.yaml", config_yaml)]);
    Ok((cm, hash))
}

/// Top-level keys that the operator pins in the rendered config. Setting any of these in
/// `spec.extraConfig` is rejected by the admission webhook — see [`check_reserved_keys`].
const RESERVED_TOP_LEVEL: &[&str] = &[
    "server_url",
    "listen_addr",
    "grpc_listen_addr",
    "grpc_allow_insecure",
    "metrics_listen_addr",
    "unix_socket",
    "unix_socket_permission",
    "noise",
    "database",
    "policy",
];

/// Subkeys under `dns` that the operator pins. The rest of `dns.*` is passed through.
const RESERVED_DNS_KEYS: &[&str] = &["magic_dns", "base_domain"];

/// Validates that `extra_config` does not attempt to set any operator-pinned keys.
/// Returns an error message listing every violation so the user can fix them in one round-trip.
pub(crate) fn check_reserved_keys(
    extra_config: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    let top: Vec<&str> = RESERVED_TOP_LEVEL
        .iter()
        .copied()
        .filter(|k| extra_config.contains_key(*k))
        .collect();

    let dns: Vec<&str> = extra_config
        .get("dns")
        .and_then(|v| v.as_object())
        .map(|user_dns| {
            RESERVED_DNS_KEYS
                .iter()
                .copied()
                .filter(|k| user_dns.contains_key(*k))
                .collect()
        })
        .unwrap_or_default();

    if top.is_empty() && dns.is_empty() {
        return Ok(());
    }

    let mut parts = Vec::new();
    if !top.is_empty() {
        parts.push(format!(
            "spec.extraConfig must not set operator-managed keys: {}",
            top.join(", ")
        ));
    }
    if !dns.is_empty() {
        parts.push(format!(
            "spec.extraConfig.dns must not set operator-managed keys: {}",
            dns.join(", ")
        ));
    }
    Err(parts.join("; "))
}

/// Renders the headscale YAML config file.
///
/// `extra_config` is the base; operator-pinned keys overwrite it. Callers are expected to
/// have already invoked [`check_reserved_keys`] (the admission webhook does this), so any
/// reserved key reaching this function is a defense-in-depth no-op rather than the primary
/// rejection path. `dns` is deep-merged (defaults → extra_config.dns → pinned operator keys)
/// so users can add `nameservers`, `split_dns`, `extra_records`, etc. `override_local_dns`
/// defaults to `false`; users may set it to `true` but must also supply `dns.nameservers.global`
/// (headscale v0.29 requirement).
pub(crate) fn build_config(
    server_url: &str,
    dns_base_domain: &str,
    extra_config: BTreeMap<String, serde_json::Value>,
) -> Result<String, serde_yaml::Error> {
    // DNS: defaults → user overrides → operator-pinned keys.
    let mut dns = serde_json::Map::new();
    dns.insert("override_local_dns".into(), false.into());
    if let Some(user_dns) = extra_config.get("dns").and_then(|v| v.as_object()) {
        dns.extend(user_dns.clone());
    }
    dns.insert("magic_dns".into(), true.into());
    dns.insert("base_domain".into(), dns_base_domain.into());

    // extra_config is the base; operator keys always overwrite (including the merged dns).
    let mut config: serde_json::Map<_, _> = extra_config.into_iter().collect();
    let serde_json::Value::Object(operator) = serde_json::json!({
        "server_url":             server_url,
        "listen_addr":            "0.0.0.0:8080",
        "grpc_listen_addr":       "0.0.0.0:50443",
        "grpc_allow_insecure":    true,
        "metrics_listen_addr":    "0.0.0.0:9090",
        "unix_socket":            "/var/run/headscale/headscale.sock",
        "unix_socket_permission": "0770",
        "noise": {
            "private_key_path": "/var/lib/headscale/noise_private.key"
        },
        "database": {
            "type":   "sqlite",
            "sqlite": {
                "path": "/var/lib/headscale/db.sqlite",
                "write_ahead_log": true
            }
        },
        "policy": {
            "mode": "database"
        },
        "dns": serde_json::Value::Object(dns),
    }) else {
        unreachable!()
    };
    config.extend(operator);

    serde_yaml::to_string(&config)
}

pub(super) fn desired_statefulset(
    name: &str,
    image: &str,
    storage: &StorageSpec,
    resources: Option<&ResourceRequirements>,
    config_hash: &str,
) -> StatefulSet {
    let config_volume = Volume::configmap("config", ConfigMapVolumeSource::new(name));
    let resources = resources
        .cloned()
        .unwrap_or_else(default_headscale_resources);
    StatefulSet::new(name)
        .replicas(1)
        .service_name(name)
        .template(
            PodTemplateSpec::new()
                .annotation("headmaster/config-hash", config_hash)
                .pod_spec(PodSpec {
                    security_context: Some(PodSecurityContext {
                        seccomp_profile: Some(SeccompProfile {
                            type_: "RuntimeDefault".into(),
                            localhost_profile: None,
                        }),
                        ..Default::default()
                    }),
                    ..PodSpec::container(
                        Container::new("headscale")
                            .image(image)
                            .args(["serve"])
                            .allow_privilege_escalation(false)
                            .drop_capabilities(["ALL"])
                            .ports([
                                ContainerPort::tcp(PORT_HTTP).name("http"),
                                ContainerPort::tcp(PORT_METRICS).name("metrics"),
                                ContainerPort::tcp(PORT_GRPC).name("grpc"),
                            ])
                            .readiness_probe(
                                Probe::http_get("/health", "http")
                                    .initial_delay_seconds(5)
                                    .period_seconds(5)
                                    .failure_threshold(3),
                            )
                            .liveness_probe(
                                Probe::http_get("/health", "http")
                                    .initial_delay_seconds(30)
                                    .period_seconds(10)
                                    .failure_threshold(3),
                            )
                            .volume_mounts([
                                VolumeMount::new("/etc/headscale/config.yaml", &config_volume)
                                    .read_only()
                                    .sub_path("config.yaml"),
                                VolumeMount {
                                    name: "data".into(),
                                    mount_path: "/var/lib/headscale".into(),
                                    ..Default::default()
                                },
                                VolumeMount {
                                    name: "var-run-headscale".into(),
                                    mount_path: "/var/run/headscale".into(),
                                    ..Default::default()
                                },
                            ])
                            .resource_requests(resources.requests.unwrap_or_default())
                            .resource_limits(resources.limits.unwrap_or_default()),
                    )
                    .volumes([
                        config_volume,
                        Volume {
                            name: "var-run-headscale".into(),
                            empty_dir: Some(EmptyDirVolumeSource::default()),
                            ..Default::default()
                        },
                    ])
                }),
        )
        .volume_claim_templates([PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("data".into()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".into()]),
                storage_class_name: storage.storage_class.clone(),
                resources: Some(VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "storage".into(),
                        Quantity(storage.size.clone()),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
}

fn default_headscale_resources() -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".into(), Quantity("50m".into())),
            ("memory".into(), Quantity("64Mi".into())),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".into(), Quantity("500m".into())),
            ("memory".into(), Quantity("512Mi".into())),
        ])),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::headscale_instance::test_support::minimal_instance;
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

    const TEST_IMAGE: &str = "ghcr.io/juanfont/headscale:v0.29.0-beta.2";

    #[test]
    fn check_reserved_keys_accepts_empty() {
        let extra_config = BTreeMap::new();
        assert!(check_reserved_keys(&extra_config).is_ok());
    }

    #[test]
    fn check_reserved_keys_accepts_passthrough_keys() {
        let extra_config = BTreeMap::from([
            ("log".into(), serde_json::json!({"level": "debug"})),
            (
                "derp".into(),
                serde_json::json!({"urls": ["https://example.com"]}),
            ),
        ]);
        assert!(check_reserved_keys(&extra_config).is_ok());
    }

    #[test]
    fn check_reserved_keys_rejects_each_top_level_reserved_key() {
        for key in RESERVED_TOP_LEVEL {
            let extra_config =
                BTreeMap::from([((*key).to_string(), serde_json::Value::String("x".into()))]);
            let err = check_reserved_keys(&extra_config)
                .expect_err(&format!("must reject reserved top-level key: {key}"));
            assert!(
                err.contains(key),
                "error message must name the violated key {key:?}: {err}"
            );
        }
    }

    #[test]
    fn check_reserved_keys_lists_all_top_level_violations_in_one_message() {
        let extra_config = BTreeMap::from([
            (
                "server_url".into(),
                serde_json::Value::String("https://attacker.example.com".into()),
            ),
            (
                "listen_addr".into(),
                serde_json::Value::String("0.0.0.0:9999".into()),
            ),
        ]);
        let err = check_reserved_keys(&extra_config).expect_err("must reject");
        assert!(err.contains("server_url"), "must name server_url: {err}");
        assert!(err.contains("listen_addr"), "must name listen_addr: {err}");
    }

    #[test]
    fn check_reserved_keys_rejects_dns_pinned_subkeys() {
        for key in RESERVED_DNS_KEYS {
            let mut dns = serde_json::Map::new();
            dns.insert((*key).into(), serde_json::Value::Bool(false));
            let extra_config = BTreeMap::from([("dns".into(), serde_json::Value::Object(dns))]);
            let err = check_reserved_keys(&extra_config)
                .expect_err(&format!("must reject reserved dns subkey: {key}"));
            assert!(
                err.contains(key),
                "error message must name the violated dns subkey {key:?}: {err}"
            );
            assert!(
                err.contains("dns"),
                "error message must scope to dns: {err}"
            );
        }
    }

    #[test]
    fn check_reserved_keys_accepts_passthrough_dns_subkeys() {
        let extra_config = BTreeMap::from([(
            "dns".into(),
            serde_json::json!({
                "nameservers": {"global": ["1.1.1.1"]},
                "override_local_dns": true,
                "extra_records": [],
            }),
        )]);
        assert!(check_reserved_keys(&extra_config).is_ok());
    }

    #[test]
    fn check_reserved_keys_reports_top_level_and_dns_together() {
        let extra_config = BTreeMap::from([
            (
                "server_url".into(),
                serde_json::Value::String("https://attacker.example.com".into()),
            ),
            (
                "dns".into(),
                serde_json::json!({"magic_dns": false, "nameservers": {"global": ["1.1.1.1"]}}),
            ),
        ]);
        let err = check_reserved_keys(&extra_config).expect_err("must reject");
        assert!(err.contains("server_url"), "must name server_url: {err}");
        assert!(err.contains("magic_dns"), "must name magic_dns: {err}");
    }

    #[test]
    fn build_config_contains_operator_keys() -> Result<(), Box<dyn std::error::Error>> {
        let obj = minimal_instance("test");
        let yaml = build_config(
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            obj.spec.extra_config.clone(),
        )?;
        assert!(yaml.contains("server_url"), "must contain server_url");
        assert!(yaml.contains("listen_addr"), "must contain listen_addr");
        assert!(yaml.contains("unix_socket"), "must contain unix_socket");
        assert!(yaml.contains("database"), "must contain database");
        Ok(())
    }

    #[test]
    fn build_config_operator_keys_win_over_extra_config() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut obj = minimal_instance("test");
        obj.spec.extra_config.insert(
            "server_url".to_string(),
            serde_json::Value::String("https://attacker.example.com".to_string()),
        );
        obj.spec.extra_config.insert(
            "listen_addr".to_string(),
            serde_json::Value::String("0.0.0.0:9999".to_string()),
        );
        let yaml = build_config(
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            obj.spec.extra_config.clone(),
        )?;
        assert!(
            yaml.contains("headscale.example.com"),
            "operator server_url must win"
        );
        assert!(
            !yaml.contains("attacker.example.com"),
            "extra_config server_url must not win"
        );
        assert!(
            yaml.contains("0.0.0.0:8080"),
            "operator listen_addr must win"
        );
        assert!(
            !yaml.contains("9999"),
            "extra_config listen_addr must not win"
        );
        Ok(())
    }

    #[test]
    fn build_config_extra_config_passes_through() -> Result<(), Box<dyn std::error::Error>> {
        let mut obj = minimal_instance("test");
        obj.spec
            .extra_config
            .insert("log".to_string(), serde_json::json!({ "level": "debug" }));
        let yaml = build_config(
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            obj.spec.extra_config.clone(),
        )?;
        assert!(
            yaml.contains("debug"),
            "extra_config log.level must pass through"
        );
        Ok(())
    }

    #[test]
    fn build_config_user_dns_keys_merge_through() -> Result<(), Box<dyn std::error::Error>> {
        let mut obj = minimal_instance("test");
        obj.spec.extra_config.insert(
            "dns".to_string(),
            serde_json::json!({
                "nameservers": {"global": ["1.1.1.1", "8.8.8.8"]},
                "override_local_dns": true,
            }),
        );
        let yaml = build_config(
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            obj.spec.extra_config.clone(),
        )?;
        assert!(
            yaml.contains("1.1.1.1"),
            "user nameservers must merge through"
        );
        assert!(
            yaml.contains("override_local_dns: true"),
            "user override_local_dns must pass through"
        );
        assert!(
            yaml.contains("magic_dns: true"),
            "operator magic_dns must be pinned even when user omits it"
        );
        Ok(())
    }

    #[test]
    fn build_config_magic_dns_cannot_be_overridden() -> Result<(), Box<dyn std::error::Error>> {
        let mut obj = minimal_instance("test");
        obj.spec
            .extra_config
            .insert("dns".to_string(), serde_json::json!({"magic_dns": false}));
        let yaml = build_config(
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            obj.spec.extra_config.clone(),
        )?;
        assert!(
            yaml.contains("magic_dns: true"),
            "operator magic_dns must always win over extra_config"
        );
        assert!(
            !yaml.contains("magic_dns: false"),
            "extra_config magic_dns: false must not win"
        );
        Ok(())
    }

    #[test]
    fn build_config_dns_base_domain_cannot_be_overridden() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut obj = minimal_instance("test");
        obj.spec.extra_config.insert(
            "dns".to_string(),
            serde_json::json!({"base_domain": "attacker.example.com"}),
        );
        let yaml = build_config(
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            obj.spec.extra_config.clone(),
        )?;
        assert!(
            yaml.contains("ts.example.com"),
            "spec base_domain must always win"
        );
        assert!(
            !yaml.contains("attacker.example.com"),
            "extra_config base_domain must not win"
        );
        Ok(())
    }

    #[test]
    fn build_config_override_local_dns_defaults_false() -> Result<(), Box<dyn std::error::Error>> {
        let obj = minimal_instance("test");
        let yaml = build_config(
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            obj.spec.extra_config.clone(),
        )?;
        assert!(
            yaml.contains("override_local_dns: false"),
            "override_local_dns must default to false to avoid headscale v0.29 nameserver requirement"
        );
        Ok(())
    }

    #[test]
    fn build_configmap_hash_is_deterministic() -> Result<(), Box<dyn std::error::Error>> {
        let obj = minimal_instance("hs");
        let (_, hash_a) = build_configmap(
            "hs-headscale",
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            &obj.spec.extra_config,
        )?;
        let (_, hash_b) = build_configmap(
            "hs-headscale",
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            &obj.spec.extra_config,
        )?;
        assert_eq!(
            hash_a, hash_b,
            "hash must be deterministic for identical inputs"
        );
        Ok(())
    }

    #[test]
    fn build_configmap_hash_changes_with_server_url() -> Result<(), Box<dyn std::error::Error>> {
        let mut obj = minimal_instance("hs");
        let (_, hash_a) = build_configmap(
            "hs-headscale",
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            &obj.spec.extra_config,
        )?;
        obj.spec.server_url = "https://other.example.com".to_string();
        let (_, hash_b) = build_configmap(
            "hs-headscale",
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            &obj.spec.extra_config,
        )?;
        assert_ne!(hash_a, hash_b, "server_url change must change the hash");
        Ok(())
    }

    #[test]
    fn build_configmap_has_config_yaml_only() -> Result<(), Box<dyn std::error::Error>> {
        let obj = minimal_instance("hs");
        let (cm, _) = build_configmap(
            "hs-headscale",
            &obj.spec.server_url,
            &obj.spec.dns_base_domain,
            &obj.spec.extra_config,
        )?;
        let data = cm.data.expect("ConfigMap must have data");
        assert!(
            data.contains_key("config.yaml"),
            "ConfigMap must have config.yaml"
        );
        assert!(
            !data.contains_key("policy.json"),
            "policy.json must not be in ConfigMap (policy uses database mode)"
        );
        Ok(())
    }

    #[test]
    fn desired_statefulset_has_config_hash_annotation() {
        let obj = minimal_instance("hs");
        let statefulset = desired_statefulset(
            "hs-headscale",
            TEST_IMAGE,
            &obj.spec.storage,
            None,
            "abc123",
        );
        let pod_annotations = statefulset
            .spec
            .unwrap()
            .template
            .metadata
            .unwrap()
            .annotations
            .unwrap();
        assert_eq!(
            pod_annotations
                .get("headmaster/config-hash")
                .map(|s| s.as_str()),
            Some("abc123"),
        );
    }

    #[test]
    fn desired_statefulset_uses_default_resources_when_none() {
        let obj = minimal_instance("hs");
        let statefulset =
            desired_statefulset("hs-headscale", TEST_IMAGE, &obj.spec.storage, None, "hash");
        let containers = statefulset.spec.unwrap().template.spec.unwrap().containers;
        let headscale_container = containers.iter().find(|c| c.name == "headscale").unwrap();
        let requests = headscale_container
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap();
        assert_eq!(requests["cpu"].0, "50m");
        assert_eq!(requests["memory"].0, "64Mi");
    }

    #[test]
    fn desired_statefulset_uses_custom_resources_when_provided() {
        let custom_resources = ResourceRequirements {
            requests: BTreeMap::from([
                ("cpu".to_string(), Quantity("200m".to_string())),
                ("memory".to_string(), Quantity("256Mi".to_string())),
            ])
            .into(),
            ..Default::default()
        };
        let obj = minimal_instance("hs");
        let statefulset = desired_statefulset(
            "hs-headscale",
            TEST_IMAGE,
            &obj.spec.storage,
            Some(&custom_resources),
            "hash",
        );
        let containers = statefulset.spec.unwrap().template.spec.unwrap().containers;
        let headscale_container = containers.iter().find(|c| c.name == "headscale").unwrap();
        let requests = headscale_container
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap();
        assert_eq!(requests["cpu"].0, "200m");
        assert_eq!(requests["memory"].0, "256Mi");
    }

    #[test]
    fn desired_statefulset_has_readiness_probe() {
        let obj = minimal_instance("hs");
        let statefulset =
            desired_statefulset("hs-headscale", TEST_IMAGE, &obj.spec.storage, None, "hash");
        let containers = statefulset.spec.unwrap().template.spec.unwrap().containers;
        let headscale_container = containers.iter().find(|c| c.name == "headscale").unwrap();
        let readiness_probe = headscale_container.readiness_probe.as_ref().unwrap();
        let http_get = readiness_probe.http_get.as_ref().unwrap();
        assert_eq!(http_get.path.as_deref(), Some("/health"));
        assert!(matches!(&http_get.port, IntOrString::String(s) if s == "http"));
        assert_eq!(readiness_probe.initial_delay_seconds, Some(5));
    }

    #[test]
    fn desired_statefulset_has_liveness_probe() {
        let obj = minimal_instance("hs");
        let statefulset =
            desired_statefulset("hs-headscale", TEST_IMAGE, &obj.spec.storage, None, "hash");
        let containers = statefulset.spec.unwrap().template.spec.unwrap().containers;
        let headscale_container = containers.iter().find(|c| c.name == "headscale").unwrap();
        let liveness_probe = headscale_container.liveness_probe.as_ref().unwrap();
        let http_get = liveness_probe.http_get.as_ref().unwrap();
        assert_eq!(http_get.path.as_deref(), Some("/health"));
        assert!(matches!(&http_get.port, IntOrString::String(s) if s == "http"));
        assert_eq!(liveness_probe.initial_delay_seconds, Some(30));
    }

    #[test]
    fn desired_statefulset_has_only_headscale_container() {
        let obj = minimal_instance("hs");
        let statefulset =
            desired_statefulset("hs-headscale", TEST_IMAGE, &obj.spec.storage, None, "hash");
        let containers = statefulset.spec.unwrap().template.spec.unwrap().containers;
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].name, "headscale");
    }

    #[test]
    fn desired_statefulset_has_seccomp_profile() {
        let obj = minimal_instance("hs");
        let statefulset =
            desired_statefulset("hs-headscale", TEST_IMAGE, &obj.spec.storage, None, "hash");
        let pod_spec = statefulset.spec.unwrap().template.spec.unwrap();
        let seccomp = pod_spec
            .security_context
            .as_ref()
            .and_then(|s| s.seccomp_profile.as_ref())
            .map(|p| p.type_.as_str());
        assert_eq!(
            seccomp,
            Some("RuntimeDefault"),
            "headscale pod must use RuntimeDefault seccomp profile"
        );
    }

    #[test]
    fn desired_statefulset_headscale_container_disallows_privilege_escalation() {
        let obj = minimal_instance("hs");
        let statefulset =
            desired_statefulset("hs-headscale", TEST_IMAGE, &obj.spec.storage, None, "hash");
        let containers = statefulset.spec.unwrap().template.spec.unwrap().containers;
        let container = containers.iter().find(|c| c.name == "headscale").unwrap();
        assert_eq!(
            container
                .security_context
                .as_ref()
                .and_then(|s| s.allow_privilege_escalation),
            Some(false),
            "headscale container must have allowPrivilegeEscalation=false"
        );
    }
}
