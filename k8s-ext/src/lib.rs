use std::fmt;

use k8s_openapi::ByteString;
use k8s_openapi::Metadata;
use k8s_openapi::api::apps::v1 as appsv1;
use k8s_openapi::api::batch::v1 as batchv1;
use k8s_openapi::api::core::v1 as corev1;
use k8s_openapi::api::rbac::v1 as rbacv1;
use k8s_openapi::apimachinery::pkg::apis::meta::v1 as metav1;
use k8s_openapi::apimachinery::pkg::util::intstr;

pub mod label;

pub use configmap::ConfigMapExt;
pub use configmap_volume_source::ConfigMapVolumeSourceExt;
pub use container::ContainerExt;
pub use container_port::ContainerPortExt;
pub use env::{EnvVarExt, ToEnvFrom, ToEnvVar};
pub use job::JobExt;
pub use pod_spec::PodSpecExt;
pub use pod_template_spec::PodTemplateSpecExt;
pub use policy_rule::PolicyRuleExt;
pub use probe::ProbeExt;
pub use role::RoleExt;
pub use role_binding::{IsRole, RoleBindingExt};
pub use secret::SecretExt;
pub use secret_env_source::SecretEnvSourceExt;
pub use secret_get::SecretGetExt;
pub use service::ServiceExt;
pub use service_account::ServiceAccountExt;
pub use service_port::ServicePortExt;
pub use statefulset::StatefulSetExt;
pub use statefulset_get::StatefulSetGetExt;
pub use subject::SubjectExt;
pub use volume::VolumeExt;
pub use volume_mount::{ToVolumeName, VolumeMountExt};

mod configmap;
mod configmap_volume_source;
mod container;
mod container_port;
mod env;
mod job;
mod pod_spec;
mod pod_template_spec;
mod policy_rule;
mod probe;
mod role;
mod role_binding;
mod secret;
mod secret_env_source;
mod secret_get;
mod service;
mod service_account;
mod service_port;
mod statefulset;
mod statefulset_get;
mod subject;
mod volume;
mod volume_mount;

pub trait ResourceBuilder: Sized {
    fn namespace(self, namespace: impl ToString) -> Self;
    fn labels(self, labels: impl IntoIterator<Item = (impl ToString, impl ToString)>) -> Self;
}

impl<T> ResourceBuilder for T
where
    T: Metadata<Ty = metav1::ObjectMeta>,
{
    fn namespace(mut self, namespace: impl ToString) -> Self {
        self.metadata_mut().namespace = Some(namespace.to_string());
        self
    }

    fn labels(mut self, labels: impl IntoIterator<Item = (impl ToString, impl ToString)>) -> Self {
        let labels = labels
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()));
        self.metadata_mut()
            .labels
            .get_or_insert_default()
            .extend(labels);
        self
    }
}

pub trait ToIntOrString {
    fn to_int_or_string(self) -> intstr::IntOrString;
}

impl ToIntOrString for i32 {
    fn to_int_or_string(self) -> intstr::IntOrString {
        intstr::IntOrString::Int(self)
    }
}

impl ToIntOrString for u16 {
    fn to_int_or_string(self) -> intstr::IntOrString {
        intstr::IntOrString::Int(self.into())
    }
}

impl ToIntOrString for &str {
    fn to_int_or_string(self) -> intstr::IntOrString {
        intstr::IntOrString::String(self.to_string())
    }
}

impl ToIntOrString for String {
    fn to_int_or_string(self) -> intstr::IntOrString {
        intstr::IntOrString::String(self)
    }
}

fn make_metadata(name: impl ToString) -> metav1::ObjectMeta {
    metav1::ObjectMeta {
        name: Some(name.to_string()),
        ..Default::default()
    }
}

trait HasSpec {
    type Spec;
    fn spec_mut(&mut self) -> &mut Self::Spec;
}
