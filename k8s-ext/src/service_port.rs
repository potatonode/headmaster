use super::*;

pub trait ServicePortExt: Sized {
    fn tcp(name: impl ToString, port: impl Into<i32>) -> Self;
    fn udp(name: impl ToString, port: impl Into<i32>) -> Self;
    fn target_port(self, port: impl ToIntOrString) -> Self;
}

impl ServicePortExt for corev1::ServicePort {
    fn tcp(name: impl ToString, port: impl Into<i32>) -> Self {
        Self {
            name: Some(name.to_string()),
            port: port.into(),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }
    }

    fn udp(name: impl ToString, port: impl Into<i32>) -> Self {
        Self {
            name: Some(name.to_string()),
            port: port.into(),
            protocol: Some("UDP".to_string()),
            ..Default::default()
        }
    }

    fn target_port(self, port: impl ToIntOrString) -> Self {
        Self {
            target_port: Some(port.to_int_or_string()),
            ..self
        }
    }
}
