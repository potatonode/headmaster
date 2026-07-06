//! Ingress controller — provisions a Tailscale proxy StatefulSet (plus
//! WireGuard NodePort Service, auth-key Secret, serve ConfigMap, RBAC) in the
//! operator namespace for every `Ingress` annotated `ingressClassName: headmaster`.
//!
//! The module is split into focused files; see each submodule's docs.

mod auth_key;
mod error;
mod names;
mod proxy;
mod reconcile;
#[cfg(test)]
mod test_support;

pub use names::ingress_auto_tag;
pub use names::{proxy_state_secret_name, proxy_sts_name};
pub use reconcile::{ensure_ingress_class, stream};

#[cfg(test)]
pub(crate) use crate::types::ANNOTATION_CONFIG;
pub(crate) use error::Error;
pub(crate) use reconcile::headscale_connect;

pub const CONTROLLER_NAME: &str = "headmaster.potatonode.github.io/ingress-controller";
pub(crate) const INGRESS_CLASS_NAME: &str = "headmaster";
