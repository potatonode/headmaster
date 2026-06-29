use super::*;

pub trait PodSpecExt: Sized {
    fn container(container: corev1::Container) -> Self;
    fn restart_policy(self, policy: impl ToString) -> Self;
    fn service_account_name(self, name: impl ToString) -> Self;
    fn volumes(self, volumes: impl IntoIterator<Item = corev1::Volume>) -> Self;
}

impl PodSpecExt for corev1::PodSpec {
    fn container(container: corev1::Container) -> Self {
        Self {
            containers: vec![container],
            ..Default::default()
        }
    }

    fn restart_policy(self, policy: impl ToString) -> Self {
        Self {
            restart_policy: Some(policy.to_string()),
            ..self
        }
    }

    fn service_account_name(self, name: impl ToString) -> Self {
        Self {
            service_account_name: Some(name.to_string()),
            ..self
        }
    }

    fn volumes(mut self, volumes: impl IntoIterator<Item = corev1::Volume>) -> Self {
        self.volumes.get_or_insert_default().extend(volumes);
        self
    }
}
