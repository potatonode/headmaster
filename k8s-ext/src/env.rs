use super::*;

pub trait EnvVarExt: Sized {
    fn value(name: impl ToString, value: impl ToString) -> Self;
    fn secret_key_ref(name: impl ToString, secret: impl ToString, key: impl ToString) -> Self;
    fn metadata_name(name: impl ToString) -> Self;
    fn metadata_namespace(name: impl ToString) -> Self;
    fn status_host_ip(name: impl ToString) -> Self;
}

impl EnvVarExt for corev1::EnvVar {
    fn value(name: impl ToString, value: impl ToString) -> Self {
        Self {
            name: name.to_string(),
            value: Some(value.to_string()),
            value_from: None,
        }
    }

    fn secret_key_ref(name: impl ToString, secret: impl ToString, key: impl ToString) -> Self {
        Self {
            name: name.to_string(),
            value: None,
            value_from: Some(corev1::EnvVarSource {
                secret_key_ref: Some(corev1::SecretKeySelector {
                    name: secret.to_string(),
                    key: key.to_string(),
                    optional: Some(false),
                }),
                ..Default::default()
            }),
        }
    }

    fn metadata_name(name: impl ToString) -> Self {
        make_field_ref_env(name, "metadata.name")
    }

    fn metadata_namespace(name: impl ToString) -> Self {
        make_field_ref_env(name, "metadata.namespace")
    }

    fn status_host_ip(name: impl ToString) -> Self {
        make_field_ref_env(name, "status.hostIP")
    }
}

fn make_field_ref_env(name: impl ToString, field_path: &str) -> corev1::EnvVar {
    let source = corev1::EnvVarSource {
        field_ref: Some(corev1::ObjectFieldSelector {
            api_version: None,
            field_path: field_path.to_string(),
        }),
        ..Default::default()
    };
    corev1::EnvVar {
        name: name.to_string(),
        value: None,
        value_from: Some(source),
    }
}

pub trait ToEnvVar {
    fn to_envvar(&self) -> corev1::EnvVar;
}

impl ToEnvVar for corev1::EnvVar {
    fn to_envvar(&self) -> corev1::EnvVar {
        self.clone()
    }
}

impl<T, U> ToEnvVar for (T, U)
where
    T: fmt::Display,
    U: fmt::Display,
{
    fn to_envvar(&self) -> corev1::EnvVar {
        let (ref name, ref value) = *self;
        corev1::EnvVar::value(name, value)
    }
}

pub trait ToEnvFrom {
    fn to_envfrom(self) -> corev1::EnvFromSource;
}

impl ToEnvFrom for corev1::SecretEnvSource {
    fn to_envfrom(self) -> corev1::EnvFromSource {
        corev1::EnvFromSource {
            secret_ref: Some(self),
            ..Default::default()
        }
    }
}

impl ToEnvFrom for corev1::ConfigMapEnvSource {
    fn to_envfrom(self) -> corev1::EnvFromSource {
        corev1::EnvFromSource {
            config_map_ref: Some(self),
            ..Default::default()
        }
    }
}
