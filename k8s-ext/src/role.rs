use super::*;

pub trait RoleExt: ResourceBuilder {
    fn new(name: impl ToString) -> Self;
    fn rules(self, rules: impl IntoIterator<Item = rbacv1::PolicyRule>) -> Self;
}

impl RoleExt for rbacv1::Role {
    fn new(name: impl ToString) -> Self {
        Self {
            metadata: make_metadata(name),
            ..Default::default()
        }
    }

    fn rules(self, rules: impl IntoIterator<Item = rbacv1::PolicyRule>) -> Self {
        Self {
            rules: Some(rules.into_iter().collect()),
            ..self
        }
    }
}
