use super::*;

pub trait SecretEnvSourceExt: Sized {
    fn required(name: impl ToString) -> Self;
}

impl SecretEnvSourceExt for corev1::SecretEnvSource {
    fn required(name: impl ToString) -> Self {
        Self {
            name: name.to_string(),
            optional: Some(false),
        }
    }
}
