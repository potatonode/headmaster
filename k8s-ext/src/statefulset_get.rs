use super::*;

pub trait StatefulSetGetExt {
    fn ready_replicas(&self) -> Option<i32>;
}

impl StatefulSetGetExt for appsv1::StatefulSet {
    fn ready_replicas(&self) -> Option<i32> {
        self.status.as_ref()?.ready_replicas
    }
}
