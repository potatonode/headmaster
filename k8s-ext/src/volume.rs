use super::*;

pub trait VolumeExt: Sized {
    fn configmap(name: impl ToString, configmap: corev1::ConfigMapVolumeSource) -> Self;
    fn emptydir(name: impl ToString) -> Self;
}

impl VolumeExt for corev1::Volume {
    fn configmap(name: impl ToString, configmap: corev1::ConfigMapVolumeSource) -> Self {
        Self {
            name: name.to_string(),
            config_map: Some(configmap),
            ..Default::default()
        }
    }

    fn emptydir(name: impl ToString) -> Self {
        Self {
            name: name.to_string(),
            empty_dir: Some(corev1::EmptyDirVolumeSource::default()),
            ..Default::default()
        }
    }
}
