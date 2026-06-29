use super::*;

pub trait IsRole: Metadata<Ty = metav1::ObjectMeta> {}
impl IsRole for rbacv1::Role {}
impl IsRole for rbacv1::ClusterRole {}

pub trait RoleBindingExt: ResourceBuilder {
    fn new<T: IsRole>(name: impl ToString, role: &T) -> Self;
    fn subjects(self, subjects: impl IntoIterator<Item = rbacv1::Subject>) -> Self;
}

impl RoleBindingExt for rbacv1::RoleBinding {
    fn new<T: IsRole>(name: impl ToString, role: &T) -> Self {
        let role_ref = rbacv1::RoleRef {
            api_group: T::GROUP.to_string(),
            kind: T::KIND.to_string(),
            name: role.metadata().name.clone().unwrap_or_default(),
        };
        Self {
            metadata: make_metadata(name),
            role_ref,
            ..Default::default()
        }
    }

    fn subjects(self, subjects: impl IntoIterator<Item = rbacv1::Subject>) -> Self {
        Self {
            subjects: Some(subjects.into_iter().collect()),
            ..self
        }
    }
}
