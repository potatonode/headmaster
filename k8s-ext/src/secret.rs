use super::*;

pub trait SecretExt: ResourceBuilder + Sized {
    fn new(name: impl ToString) -> Self;
    fn data(self, data: impl IntoIterator<Item = (impl ToString, ByteString)>) -> Self;
    fn string_data(self, data: impl IntoIterator<Item = (impl ToString, impl ToString)>) -> Self;
}

impl SecretExt for corev1::Secret {
    fn new(name: impl ToString) -> Self {
        Self {
            metadata: make_metadata(name),
            ..Default::default()
        }
    }

    fn data(mut self, data: impl IntoIterator<Item = (impl ToString, ByteString)>) -> Self {
        let iter = data
            .into_iter()
            .map(|(key, value)| (key.to_string(), value));
        self.data.get_or_insert_default().extend(iter);
        self
    }

    fn string_data(
        mut self,
        data: impl IntoIterator<Item = (impl ToString, impl ToString)>,
    ) -> Self {
        let iter = data
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()));
        self.string_data.get_or_insert_default().extend(iter);
        self
    }
}
