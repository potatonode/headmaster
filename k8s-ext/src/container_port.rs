use super::*;

pub trait ContainerPortExt: Sized {
    fn tcp(port: impl Into<i32>) -> Self;
    fn name(self, name: impl ToString) -> Self;
}

impl ContainerPortExt for corev1::ContainerPort {
    fn tcp(port: impl Into<i32>) -> Self {
        Self {
            container_port: port.into(),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }
    }

    fn name(self, name: impl ToString) -> Self {
        Self {
            name: Some(name.to_string()),
            ..self
        }
    }
}
