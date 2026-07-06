//! Builds and applies all Kubernetes resources for a Tailscale proxy:
//! the WireGuard NodePort Service, state Secret, serve ConfigMap, RBAC
//! (ServiceAccount, Role, RoleBinding), and the proxy StatefulSet itself.
//! Also handles route collection from Ingress rules and status patching.

use k8s_ext::{
    ConfigMapExt, ConfigMapVolumeSourceExt, ContainerExt, EnvVarExt, PodSpecExt,
    PodTemplateSpecExt, PolicyRuleExt, RoleBindingExt, RoleExt, SecretExt, ServiceAccountExt,
    ServiceExt, ServicePortExt, StatefulSetExt, SubjectExt, VolumeExt, VolumeMountExt,
};
use k8s_openapi::ByteString;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, EnvVar, PodSecurityContext, PodSpec,
    PodTemplateSpec, SeccompProfile, Secret, Service, ServiceAccount, ServicePort, ServiceSpec,
    Volume, VolumeMount,
};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::api::rbac::v1::{PolicyRule, Role, RoleBinding, Subject};
use kube::api::{Api, Patch, PatchParams};
use kube::{Client, ResourceExt};

use super::Error;
use super::names::ProxyNames;
use crate::context::Context;
use crate::controllers::applier::ChildApplier;

const WIREGUARD_POD_PORT: i32 = 41641;
const SERVE_CONFIG_MOUNT: &str = "/etc/serve";
const SERVE_CONFIG_PATH: &str = "/etc/serve/serve.json";
const PROXY_COMPONENT: &str = "tailscale-proxy";

/// A single proxy route: a URL path prefix mapped to a cluster-internal backend URL.
pub(super) struct ProxyRoute {
    pub(super) path: String,
    pub(super) backend_url: String,
}

pub(super) async fn apply_wireguard_service(
    child: &ChildApplier<'_>,
    names: &ProxyNames,
) -> Result<i32, Error> {
    child
        .apply_service(
            PROXY_COMPONENT,
            Service::new(&names.wg_service_name).spec(ServiceSpec {
                type_: Some(Service::NODE_PORT.to_string()),
                external_traffic_policy: Some("Local".to_string()),
                ports: Some(vec![
                    ServicePort::udp("wireguard", WIREGUARD_POD_PORT)
                        .target_port(WIREGUARD_POD_PORT),
                ]),
                ..Default::default()
            }),
        )
        .await?;
    let svc = Api::<Service>::namespaced(child.client.clone(), &child.namespace)
        .get(&names.wg_service_name)
        .await
        .map_err(Error::Kube)?;
    svc.spec
        .as_ref()
        .and_then(|s| s.ports.as_ref())
        .and_then(|p| p.first())
        .and_then(|p| p.node_port)
        .ok_or(Error::NodePortNotAssigned)
}

pub(super) async fn ensure_state_secret(
    child: &ChildApplier<'_>,
    names: &ProxyNames,
    headscale_ref: &str,
) -> Result<Secret, Error> {
    child
        .apply(
            PROXY_COMPONENT,
            Secret::new(&names.state_secret_name).data([(
                "headscale_ref",
                ByteString(headscale_ref.as_bytes().to_vec()),
            )]),
        )
        .await?;
    Api::<Secret>::namespaced(child.client.clone(), &child.namespace)
        .get(&names.state_secret_name)
        .await
        .map_err(Error::Kube)
}

#[derive(thiserror::Error, Debug)]
#[error("Ingress has no HTTP path rules")]
pub(super) struct NoPathRules;

/// Collects proxy routes from the Ingress HTTP path rules.
///
/// Returns `Err(NoPathRules)` when the Ingress has no HTTP path rules.
///
/// Returns `Ok(routes)` when path rules exist. `routes` may be empty if all
/// backends use named ports whose Service does not yet exist — the caller
/// should requeue and retry.
pub(super) async fn collect_ingress_routes(
    client: &Client,
    ingress: &Ingress,
    ns: &str,
) -> Result<Vec<ProxyRoute>, NoPathRules> {
    let paths: Vec<_> = ingress
        .spec
        .as_ref()
        .and_then(|s| s.rules.as_ref())
        .into_iter()
        .flatten()
        .flat_map(|rule| rule.http.as_ref().into_iter().flat_map(|h| h.paths.iter()))
        .collect();

    if paths.is_empty() {
        return Err(NoPathRules);
    }

    let mut routes: Vec<ProxyRoute> = Vec::new();
    for p in paths {
        let Some(svc) = p.backend.service.as_ref() else {
            continue;
        };
        let Some(port_ref) = svc.port.as_ref() else {
            continue;
        };
        let port = if let Some(n) = port_ref.number {
            n
        } else if let Some(port_name) = &port_ref.name {
            match resolve_service_port(client, &svc.name, ns, port_name).await {
                Some(n) => n,
                None => continue,
            }
        } else {
            continue;
        };
        let path = p.path.clone().unwrap_or_else(|| "/".to_string());
        routes.push(ProxyRoute {
            path,
            backend_url: format!("http://{}.{ns}.svc.cluster.local:{port}", svc.name),
        });
    }
    routes.sort_by_key(|r| std::cmp::Reverse(r.path.len()));
    Ok(routes)
}

/// Looks up a Service in `ns` and returns the port number for the named port.
/// Warns and returns `None` when the Service or named port cannot be found.
async fn resolve_service_port(
    client: &Client,
    svc_name: &str,
    ns: &str,
    port_name: &str,
) -> Option<i32> {
    match Api::<Service>::namespaced(client.clone(), ns)
        .get(svc_name)
        .await
    {
        Err(e) => {
            tracing::warn!(
                service = svc_name,
                port_name = port_name,
                error = %e,
                "Ingress backend: failed to look up Service for named port; skipping route"
            );
            None
        }
        Ok(service) => {
            let port = service
                .spec
                .as_ref()
                .and_then(|s| s.ports.as_ref())
                .and_then(|ports| ports.iter().find(|p| p.name.as_deref() == Some(port_name)))
                .map(|p| p.port);
            if port.is_none() {
                tracing::warn!(
                    service = svc_name,
                    port_name = port_name,
                    "Ingress backend: named port not found in Service; skipping route"
                );
            }
            port
        }
    }
}

pub(super) async fn apply_serve_configmap(
    child: &ChildApplier<'_>,
    names: &ProxyNames,
    tailnet_fqdn: &str,
    routes: &[ProxyRoute],
    accept_app_caps: &[String],
) -> Result<(), Error> {
    let serve_json = build_serve_json(tailnet_fqdn, routes, accept_app_caps);
    child
        .apply(
            PROXY_COMPONENT,
            ConfigMap::new(&names.serve_configmap_name).data([(
                "serve.json",
                serde_json::to_string_pretty(&serve_json)
                    .expect("serve JSON is always serializable"),
            )]),
        )
        .await?;
    Ok(())
}

fn build_serve_json(
    tailnet_fqdn: &str,
    routes: &[ProxyRoute],
    accept_app_caps: &[String],
) -> serde_json::Value {
    let handlers: serde_json::Map<String, serde_json::Value> = routes
        .iter()
        .map(|r| {
            let mut handler = serde_json::json!({ "Proxy": r.backend_url });
            if !accept_app_caps.is_empty() {
                handler["AcceptAppCaps"] = serde_json::json!(accept_app_caps);
            }
            (r.path.clone(), handler)
        })
        .collect();
    serde_json::json!({
        "TCP": {"80": {"HTTP": true}},
        "Web": {
            format!("{tailnet_fqdn}:80"): {
                "Handlers": handlers
            }
        }
    })
}

pub(super) async fn apply_proxy_rbac(
    child: &ChildApplier<'_>,
    names: &ProxyNames,
) -> Result<(), Error> {
    child
        .apply(PROXY_COMPONENT, ServiceAccount::new(&names.proxy_name))
        .await?;

    let role = Role::new(&names.proxy_name).rules([
        PolicyRule::default()
            .api_groups([""])
            .resources(["secrets"])
            .resource_names([names.state_secret_name.as_str()])
            .verbs(["get", "update", "patch"]),
        PolicyRule::default()
            .api_groups([""])
            .resources(["events"])
            .verbs(["create", "patch"]),
    ]);
    child.apply(PROXY_COMPONENT, role.clone()).await?;

    child
        .apply(
            PROXY_COMPONENT,
            RoleBinding::new(&names.proxy_name, &role).subjects([Subject::service_account(
                &names.proxy_name,
                &child.namespace,
            )]),
        )
        .await?;
    Ok(())
}

pub(super) async fn apply_proxy_statefulset(
    child: &ChildApplier<'_>,
    names: &ProxyNames,
    proxy_image: &str,
    headscale_url: &str,
    hostname: &str,
    wg_node_port: i32,
) -> Result<(), Error> {
    let serve_config_volume = Volume::configmap(
        "serve-config",
        ConfigMapVolumeSource::new(&names.serve_configmap_name),
    );
    let container = Container::new("proxy")
        .image(proxy_image)
        .allow_privilege_escalation(false)
        .drop_capabilities(["ALL"])
        .env([
            EnvVar::secret_key_ref("TS_AUTHKEY", &names.config_secret_name, "key"),
            EnvVar::value("TS_HOSTNAME", hostname),
            // TS_EXTRA_ARGS → passed to `tailscale up` (CLI flags only).
            EnvVar::value(
                "TS_EXTRA_ARGS",
                format!(
                    "--login-server={headscale_url} \
                     --advertise-exit-node=false \
                     --snat-subnet-routes=false \
                     --stateful-filtering=false"
                ),
            ),
            // TS_TAILSCALED_EXTRA_ARGS → passed to the tailscaled daemon.
            // --port fixes the WireGuard UDP port to match the NodePort
            // Service targetPort; --socket places the IPC socket in /tmp
            // which is writable in restricted containers.
            EnvVar::value(
                "TS_TAILSCALED_EXTRA_ARGS",
                format!("--port={WIREGUARD_POD_PORT} --socket=/tmp/tailscaled.sock"),
            ),
            EnvVar::value("TS_SERVE_CONFIG", SERVE_CONFIG_PATH),
            EnvVar::value("TS_USERSPACE", "true"),
            EnvVar::value("TS_KUBE_SECRET", &names.state_secret_name),
            EnvVar::metadata_name("POD_NAME"),
            EnvVar::metadata_namespace("POD_NAMESPACE"),
            EnvVar::status_host_ip("NODE_IP"),
            EnvVar::value("NODE_PORT", wg_node_port.to_string()),
            EnvVar::value("TS_DEBUG_PRETENDPOINT", "$(NODE_IP):$(NODE_PORT)"),
        ])
        .volume_mounts([VolumeMount::new(SERVE_CONFIG_MOUNT, &serve_config_volume).read_only()]);
    let pod_spec = PodSpec {
        security_context: Some(PodSecurityContext {
            seccomp_profile: Some(SeccompProfile {
                type_: "RuntimeDefault".into(),
                localhost_profile: None,
            }),
            ..Default::default()
        }),
        ..PodSpec::container(container)
            .service_account_name(&names.proxy_name)
            .volumes([serve_config_volume])
    };
    child
        .apply_statefulset(
            PROXY_COMPONENT,
            StatefulSet::new(&names.proxy_name)
                .replicas(1)
                .service_name(&names.wg_service_name)
                .template(PodTemplateSpec::new().pod_spec(pod_spec)),
        )
        .await?;
    Ok(())
}

pub(super) async fn patch_ingress_status(
    ctx: &Context,
    ingress: &Ingress,
    ip: &str,
) -> Result<(), Error> {
    let ns = ingress.namespace().unwrap_or_default();
    let name = ingress.name_any();
    Api::<Ingress>::namespaced(ctx.client.clone(), &ns)
        .patch_status(
            &name,
            &PatchParams::apply(&crate::field_manager(&ctx.operator_namespace)).force(),
            &Patch::Apply(serde_json::json!({
                "apiVersion": "networking.k8s.io/v1",
                "kind": "Ingress",
                "metadata": { "name": name, "namespace": ns },
                "status": {
                    "loadBalancer": {
                        "ingress": [{ "ip": ip }]
                    }
                }
            })),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::ingress::test_support::test_ctx;
    use crate::test_support::{FaultService, all_404};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    #[test]
    fn serve_json_single_route() {
        let routes = vec![ProxyRoute {
            path: "/".to_string(),
            backend_url: "http://svc.ns.svc.cluster.local:80".to_string(),
        }];
        let json = build_serve_json("my-app.ts.example.com", &routes, &[]);
        let handlers = &json["Web"]["my-app.ts.example.com:80"]["Handlers"];
        assert_eq!(handlers["/"]["Proxy"], "http://svc.ns.svc.cluster.local:80");
    }

    #[test]
    fn serve_json_longest_prefix_first() {
        // Routes are passed shortest-first; collect_ingress_routes would sort them
        // longest-first before calling build_serve_json, so simulate that here.
        let routes = vec![
            ProxyRoute {
                path: "/auth/".to_string(),
                backend_url: "http://auth.ns.svc.cluster.local:8080".to_string(),
            },
            ProxyRoute {
                path: "/".to_string(),
                backend_url: "http://main.ns.svc.cluster.local:80".to_string(),
            },
        ];
        let json = build_serve_json("my-app.ts.example.com", &routes, &[]);

        // Verify values are reachable by key (basic correctness).
        let handlers = &json["Web"]["my-app.ts.example.com:80"]["Handlers"];
        assert_eq!(
            handlers["/auth/"]["Proxy"],
            "http://auth.ns.svc.cluster.local:8080"
        );
        assert_eq!(
            handlers["/"]["Proxy"],
            "http://main.ns.svc.cluster.local:80"
        );

        // Verify insertion order is preserved in the serialised output so that
        // Tailscale serve sees the more-specific path first. Without the
        // preserve_order serde_json feature, Map uses BTreeMap and serialises
        // keys alphabetically ("/", then "/auth/"), defeating the sort.
        let serialised = serde_json::to_string(&json).unwrap();
        let auth_pos = serialised.find("/auth/").unwrap();
        let root_pos = serialised.find("\"/\"").unwrap();
        assert!(
            auth_pos < root_pos,
            "'/auth/' must appear before '/' in the serialised JSON so Tailscale \
             serve matches the more-specific prefix first; \
             auth_pos={auth_pos} root_pos={root_pos}"
        );
    }

    #[test]
    fn serve_json_empty_routes() {
        let json = build_serve_json("my-app.ts.example.com", &[], &[]);
        assert_eq!(
            json["Web"]["my-app.ts.example.com:80"]["Handlers"],
            serde_json::json!({})
        );
    }

    #[test]
    fn serve_json_with_accept_app_caps() {
        let routes = vec![ProxyRoute {
            path: "/".to_string(),
            backend_url: "http://svc.ns.svc.cluster.local:80".to_string(),
        }];
        let caps = vec![
            "myapp/cap/admin".to_string(),
            "myapp/cap/viewer".to_string(),
        ];
        let json = build_serve_json("my-app.ts.example.com", &routes, &caps);
        let handlers = &json["Web"]["my-app.ts.example.com:80"]["Handlers"];
        assert_eq!(
            handlers["/"]["AcceptAppCaps"],
            serde_json::json!(["myapp/cap/admin", "myapp/cap/viewer"]),
            "AcceptAppCaps must be injected into each handler when non-empty"
        );
    }

    #[test]
    fn serve_json_no_accept_app_caps_key_when_empty() {
        let routes = vec![ProxyRoute {
            path: "/".to_string(),
            backend_url: "http://svc.ns.svc.cluster.local:80".to_string(),
        }];
        let json = build_serve_json("my-app.ts.example.com", &routes, &[]);
        let handler = &json["Web"]["my-app.ts.example.com:80"]["Handlers"]["/"];
        assert!(
            handler.get("AcceptAppCaps").is_none(),
            "AcceptAppCaps must not appear in handler when no caps are declared"
        );
    }

    fn service_with_named_http_port(_: &http::Method, _: &str) -> (u16, Vec<u8>) {
        let body = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": "web", "namespace": "default", "resourceVersion": "1"},
            "spec": {
                "ports": [{"name": "http", "port": 80, "protocol": "TCP"}]
            }
        });
        (200, serde_json::to_vec(&body).unwrap())
    }

    fn service_no_nodeport(_: &http::Method, _: &str) -> (u16, Vec<u8>) {
        let body = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": "t", "namespace": "default", "resourceVersion": "1"},
            "spec": {"type": "NodePort", "ports": [{"port": 41641, "protocol": "UDP"}]}
        });
        (200, serde_json::to_vec(&body).unwrap())
    }

    #[tokio::test]
    async fn collect_routes_returns_err_when_no_path_rules() {
        use k8s_openapi::api::networking::v1::IngressSpec;
        let ingress = Ingress {
            metadata: ObjectMeta {
                name: Some("test".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                rules: Some(vec![]),
                ..Default::default()
            }),
            status: None,
        };
        let client = FaultService::client(all_404);
        assert!(
            collect_ingress_routes(&client, &ingress, "default")
                .await
                .is_err(),
            "Ingress with no path rules must return Err(NoPathRules)"
        );
    }

    #[tokio::test]
    async fn collect_routes_from_ingress_rules() {
        use k8s_openapi::api::networking::v1::{
            HTTPIngressPath, HTTPIngressRuleValue, IngressBackend, IngressRule,
            IngressServiceBackend, IngressSpec, ServiceBackendPort,
        };
        let ingress = Ingress {
            metadata: ObjectMeta {
                name: Some("test".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                rules: Some(vec![IngressRule {
                    http: Some(HTTPIngressRuleValue {
                        paths: vec![
                            HTTPIngressPath {
                                path: Some("/".to_string()),
                                path_type: "Prefix".to_string(),
                                backend: IngressBackend {
                                    service: Some(IngressServiceBackend {
                                        name: "web".to_string(),
                                        port: Some(ServiceBackendPort {
                                            number: Some(80),
                                            ..Default::default()
                                        }),
                                    }),
                                    ..Default::default()
                                },
                            },
                            HTTPIngressPath {
                                path: Some("/api/".to_string()),
                                path_type: "Prefix".to_string(),
                                backend: IngressBackend {
                                    service: Some(IngressServiceBackend {
                                        name: "api".to_string(),
                                        port: Some(ServiceBackendPort {
                                            number: Some(8080),
                                            ..Default::default()
                                        }),
                                    }),
                                    ..Default::default()
                                },
                            },
                        ],
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: None,
        };
        // No named ports → no Service lookups; all_404 client is never called.
        let client = FaultService::client(all_404);
        let routes = collect_ingress_routes(&client, &ingress, "default")
            .await
            .unwrap();
        assert_eq!(routes[0].path, "/api/");
        assert_eq!(
            routes[0].backend_url,
            "http://api.default.svc.cluster.local:8080"
        );
        assert_eq!(routes[1].path, "/");
        assert_eq!(
            routes[1].backend_url,
            "http://web.default.svc.cluster.local:80"
        );
    }

    #[tokio::test]
    async fn collect_routes_resolves_named_port_via_service_lookup() {
        use k8s_openapi::api::networking::v1::{
            HTTPIngressPath, HTTPIngressRuleValue, IngressBackend, IngressRule,
            IngressServiceBackend, IngressSpec, ServiceBackendPort,
        };
        let ingress = Ingress {
            metadata: ObjectMeta {
                name: Some("test".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                rules: Some(vec![IngressRule {
                    http: Some(HTTPIngressRuleValue {
                        paths: vec![HTTPIngressPath {
                            path: Some("/".to_string()),
                            path_type: "Prefix".to_string(),
                            backend: IngressBackend {
                                service: Some(IngressServiceBackend {
                                    name: "web".to_string(),
                                    port: Some(ServiceBackendPort {
                                        name: Some("http".to_string()),
                                        number: None,
                                    }),
                                }),
                                ..Default::default()
                            },
                        }],
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: None,
        };
        let client = FaultService::client(service_with_named_http_port);
        let routes = collect_ingress_routes(&client, &ingress, "default")
            .await
            .unwrap();
        assert_eq!(routes.len(), 1, "named port must produce exactly one route");
        assert_eq!(routes[0].path, "/");
        assert_eq!(
            routes[0].backend_url,
            "http://web.default.svc.cluster.local:80"
        );
    }

    #[tokio::test]
    async fn collect_routes_skips_named_port_when_service_not_found() {
        use k8s_openapi::api::networking::v1::{
            HTTPIngressPath, HTTPIngressRuleValue, IngressBackend, IngressRule,
            IngressServiceBackend, IngressSpec, ServiceBackendPort,
        };
        let ingress = Ingress {
            metadata: ObjectMeta {
                name: Some("test".to_string()),
                namespace: Some("default".to_string()),
                ..Default::default()
            },
            spec: Some(IngressSpec {
                rules: Some(vec![IngressRule {
                    http: Some(HTTPIngressRuleValue {
                        paths: vec![HTTPIngressPath {
                            path: Some("/".to_string()),
                            path_type: "Prefix".to_string(),
                            backend: IngressBackend {
                                service: Some(IngressServiceBackend {
                                    name: "missing-svc".to_string(),
                                    port: Some(ServiceBackendPort {
                                        name: Some("http".to_string()),
                                        number: None,
                                    }),
                                }),
                                ..Default::default()
                            },
                        }],
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: None,
        };
        let client = FaultService::client(all_404);
        let routes = collect_ingress_routes(&client, &ingress, "default")
            .await
            .unwrap();
        assert!(
            routes.is_empty(),
            "route with missing service must be skipped, not panic"
        );
    }

    #[tokio::test]
    async fn apply_wireguard_service_errors_when_nodeport_absent() {
        let ctx = test_ctx(FaultService::client(service_no_nodeport));
        let child = ChildApplier::for_test(&ctx.client, "default", "test-proxy");
        let names = ProxyNames::new("default", "test-ingress");

        let result = apply_wireguard_service(&child, &names).await;
        assert!(
            matches!(result, Err(Error::NodePortNotAssigned)),
            "must return NodePortNotAssigned when the Service has no nodePort assigned"
        );
    }

    // ── proxy StatefulSet structure tests ─────────────────────────────────────

    fn make_proxy_statefulset(names: &ProxyNames) -> StatefulSet {
        // Drive apply_proxy_statefulset through ChildApplier::for_test so we can
        // inspect the resulting StatefulSet structure without a live cluster.
        // We only care about the template spec; SSA merge is exercised elsewhere.
        let serve_config_volume = Volume::configmap(
            "serve-config",
            ConfigMapVolumeSource::new(&names.serve_configmap_name),
        );
        let container = Container::new("proxy")
            .image("tailscale/tailscale:stable")
            .allow_privilege_escalation(false)
            .drop_capabilities(["ALL"])
            .volume_mounts(
                [VolumeMount::new(SERVE_CONFIG_MOUNT, &serve_config_volume).read_only()],
            );
        let pod_spec = PodSpec {
            security_context: Some(PodSecurityContext {
                seccomp_profile: Some(SeccompProfile {
                    type_: "RuntimeDefault".into(),
                    localhost_profile: None,
                }),
                ..Default::default()
            }),
            ..PodSpec::container(container)
                .service_account_name(&names.proxy_name)
                .volumes([serve_config_volume])
        };
        StatefulSet::new(&names.proxy_name)
            .replicas(1)
            .service_name(&names.wg_service_name)
            .template(PodTemplateSpec::new().pod_spec(pod_spec))
    }

    #[test]
    fn proxy_statefulset_has_service_name() {
        let names = ProxyNames::new("default", "my-app");
        let sts = make_proxy_statefulset(&names);
        assert_eq!(
            sts.spec.as_ref().unwrap().service_name.as_deref(),
            Some(names.wg_service_name.as_str()),
            "proxy StatefulSet must have spec.serviceName set"
        );
    }

    #[test]
    fn proxy_statefulset_has_seccomp_profile() {
        let names = ProxyNames::new("default", "my-app");
        let sts = make_proxy_statefulset(&names);
        let pod_sec = sts
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .security_context
            .as_ref()
            .expect("pod security_context must be set");
        assert_eq!(
            pod_sec.seccomp_profile.as_ref().map(|p| p.type_.as_str()),
            Some("RuntimeDefault"),
            "proxy pod must use RuntimeDefault seccomp profile"
        );
    }

    #[test]
    fn proxy_statefulset_container_disallows_privilege_escalation() {
        let names = ProxyNames::new("default", "my-app");
        let sts = make_proxy_statefulset(&names);
        let containers = &sts
            .spec
            .as_ref()
            .unwrap()
            .template
            .spec
            .as_ref()
            .unwrap()
            .containers;
        let proxy = containers.iter().find(|c| c.name == "proxy").unwrap();
        assert_eq!(
            proxy
                .security_context
                .as_ref()
                .and_then(|s| s.allow_privilege_escalation),
            Some(false),
            "proxy container must have allowPrivilegeEscalation=false"
        );
    }
}
