use super::*;

pub trait ToVolumeName {
    fn volume_name(self) -> String;
}

impl ToVolumeName for &str {
    fn volume_name(self) -> String {
        self.to_string()
    }
}

impl ToVolumeName for String {
    fn volume_name(self) -> String {
        self
    }
}

impl ToVolumeName for &String {
    fn volume_name(self) -> String {
        self.clone()
    }
}

impl ToVolumeName for &corev1::Volume {
    fn volume_name(self) -> String {
        self.name.clone()
    }
}

pub trait VolumeMountExt: Sized {
    fn new(mount_path: impl ToString, volume: impl ToVolumeName) -> Self;
    fn read_only(self) -> Self;
    fn sub_path(self, path: impl ToString) -> Self;
}

impl VolumeMountExt for corev1::VolumeMount {
    fn new(mount_path: impl ToString, volume: impl ToVolumeName) -> Self {
        Self {
            mount_path: mount_path.to_string(),
            name: volume.volume_name(),
            ..Default::default()
        }
    }

    fn read_only(mut self) -> Self {
        self.read_only = Some(true);
        self
    }

    fn sub_path(mut self, path: impl ToString) -> Self {
        self.sub_path = Some(path.to_string());
        self
    }
}
