use super::*;

pub trait ConfigMapVolumeSourceExt: Sized {
    fn new(name: impl ToString) -> Self;
}

impl ConfigMapVolumeSourceExt for corev1::ConfigMapVolumeSource {
    fn new(name: impl ToString) -> Self {
        Self {
            name: name.to_string(),
            ..Default::default()
        }
    }
}
