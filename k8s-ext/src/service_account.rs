use super::*;

pub trait ServiceAccountExt: ResourceBuilder {
    fn new(name: impl ToString) -> Self;
}

impl ServiceAccountExt for corev1::ServiceAccount {
    fn new(name: impl ToString) -> Self {
        Self {
            metadata: make_metadata(name),
            ..Default::default()
        }
    }
}
