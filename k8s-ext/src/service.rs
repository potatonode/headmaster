use super::*;

pub trait ServiceExt: ResourceBuilder {
    const NODE_PORT: &str = "NodePort";

    fn new(name: impl ToString) -> Self;
    fn spec(self, spec: corev1::ServiceSpec) -> Self;
}

impl ServiceExt for corev1::Service {
    fn new(name: impl ToString) -> Self {
        Self {
            metadata: make_metadata(name),
            ..Default::default()
        }
    }

    fn spec(self, spec: corev1::ServiceSpec) -> Self {
        Self {
            spec: Some(spec),
            ..self
        }
    }
}
