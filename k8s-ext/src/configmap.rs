use super::*;

pub trait ConfigMapExt: ResourceBuilder {
    fn new(name: impl ToString) -> Self;
    fn data(self, data: impl IntoIterator<Item = (impl ToString, impl ToString)>) -> Self;
}

impl ConfigMapExt for corev1::ConfigMap {
    fn new(name: impl ToString) -> Self {
        Self {
            metadata: make_metadata(name),
            ..Default::default()
        }
    }

    fn data(self, data: impl IntoIterator<Item = (impl ToString, impl ToString)>) -> Self {
        let data = data
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Self {
            data: Some(data),
            ..self
        }
    }
}
