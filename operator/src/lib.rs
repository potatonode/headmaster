use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;

pub const GROUP: &str = "headmaster.potatonode.github.io";
pub const FIELD_MANAGER: &str = "headmaster";
pub const FINALIZER: &str = "headmaster.potatonode.github.io/cleanup";

pub mod labels {
    pub use k8s_openapi_ext::label::{APP_INSTANCE, APP_MANAGED_BY, APP_NAME};

    pub const MANAGED_BY_VALUE: &str = "headmaster";
    pub const INGRESS_NAME: &str = "headmaster.potatonode.github.io/ingress-name";
    pub const INGRESS_NAMESPACE: &str = "headmaster.potatonode.github.io/ingress-namespace";
    pub fn managed_by_selector() -> String {
        format!("{}={}", APP_MANAGED_BY, MANAGED_BY_VALUE)
    }
}

pub mod context;
pub mod controllers;
pub mod server;
pub mod types;

#[cfg(test)]
pub(crate) mod test_support;

/// Returns CRDs owned by this operator.
///
/// Used by `crdgen` to emit manifests and by functional tests to register
/// operator-managed CRDs with envtest.
pub fn crds() -> Vec<CustomResourceDefinition> {
    use kube::CustomResourceExt;
    vec![types::HeadscaleInstance::crd()]
}
