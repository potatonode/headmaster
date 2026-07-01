//! HeadscaleInstance controller — manages the headscale StatefulSet,
//! Service, ConfigMap, API-key bootstrap, optional SCIM sidecar, and policy
//! sync for each `HeadscaleInstance` CR.

mod bootstrap;
mod builders;
mod error;
mod policy;
mod reconcile;
mod scim;
#[cfg(test)]
mod test_support;

pub use reconcile::stream;

pub(crate) use builders::{build_config, check_reserved_keys};
pub(crate) use error::Error;
pub(crate) use policy::policy_has_groups_with_members;

// Container ports for the headscale pod.
const PORT_HTTP: i32 = 8080;
const PORT_METRICS: i32 = 9090;
const PORT_GRPC: i32 = 50443;

pub(super) const PORT_SCIM: i32 = 8081;
