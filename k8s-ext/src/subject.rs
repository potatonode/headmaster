use super::*;

pub trait SubjectExt: Sized {
    fn service_account(name: impl ToString, namespace: impl ToString) -> Self;
}

impl SubjectExt for rbacv1::Subject {
    fn service_account(name: impl ToString, namespace: impl ToString) -> Self {
        Self {
            kind: "ServiceAccount".to_string(),
            name: name.to_string(),
            namespace: Some(namespace.to_string()),
            api_group: Some("".to_string()),
        }
    }
}
