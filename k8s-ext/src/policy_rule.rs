use super::*;

pub trait PolicyRuleExt: Sized {
    fn api_groups(self, groups: impl IntoIterator<Item = impl ToString>) -> Self;
    fn resources(self, resources: impl IntoIterator<Item = impl ToString>) -> Self;
    fn resource_names(self, names: impl IntoIterator<Item = impl ToString>) -> Self;
    fn verbs(self, verbs: impl IntoIterator<Item = impl ToString>) -> Self;
}

impl PolicyRuleExt for rbacv1::PolicyRule {
    fn api_groups(self, groups: impl IntoIterator<Item = impl ToString>) -> Self {
        Self {
            api_groups: Some(groups.into_iter().map(|g| g.to_string()).collect()),
            ..self
        }
    }

    fn resources(self, resources: impl IntoIterator<Item = impl ToString>) -> Self {
        Self {
            resources: Some(resources.into_iter().map(|r| r.to_string()).collect()),
            ..self
        }
    }

    fn resource_names(self, names: impl IntoIterator<Item = impl ToString>) -> Self {
        Self {
            resource_names: Some(names.into_iter().map(|n| n.to_string()).collect()),
            ..self
        }
    }

    fn verbs(self, verbs: impl IntoIterator<Item = impl ToString>) -> Self {
        Self {
            verbs: verbs.into_iter().map(|v| v.to_string()).collect(),
            ..self
        }
    }
}
