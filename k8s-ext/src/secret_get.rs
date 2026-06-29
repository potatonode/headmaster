use super::*;

pub trait SecretGetExt {
    fn item(&self, key: &str) -> Option<&ByteString>;
}

impl SecretGetExt for corev1::Secret {
    fn item(&self, key: &str) -> Option<&ByteString> {
        self.data.as_ref()?.get(key)
    }
}
