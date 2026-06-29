use super::*;

pub trait PodTemplateSpecExt: Sized {
    fn new() -> Self;
    fn annotation(self, key: impl ToString, value: impl ToString) -> Self;
    fn pod_spec(self, spec: corev1::PodSpec) -> Self;
}

impl PodTemplateSpecExt for corev1::PodTemplateSpec {
    fn new() -> Self {
        Self {
            metadata: None,
            spec: None,
        }
    }

    fn annotation(mut self, key: impl ToString, value: impl ToString) -> Self {
        self.metadata
            .get_or_insert_default()
            .annotations
            .get_or_insert_default()
            .insert(key.to_string(), value.to_string());
        self
    }

    fn pod_spec(self, spec: corev1::PodSpec) -> Self {
        Self {
            spec: Some(spec),
            ..self
        }
    }
}
