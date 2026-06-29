use super::*;

pub trait JobExt: ResourceBuilder + Sized {
    fn new(name: impl ToString) -> Self;
    fn backoff_limit(self, limit: i32) -> Self;
    fn template(self, template: corev1::PodTemplateSpec) -> Self;
    fn ttl_seconds_after_finished(self, seconds: i32) -> Self;
}

impl JobExt for batchv1::Job {
    fn new(name: impl ToString) -> Self {
        Self {
            metadata: make_metadata(name),
            ..Default::default()
        }
    }

    fn backoff_limit(mut self, limit: i32) -> Self {
        self.spec_mut().backoff_limit.replace(limit);
        self
    }

    fn template(mut self, template: corev1::PodTemplateSpec) -> Self {
        self.spec_mut().template = template;
        self
    }

    fn ttl_seconds_after_finished(mut self, seconds: i32) -> Self {
        self.spec_mut().ttl_seconds_after_finished.replace(seconds);
        self
    }
}

impl HasSpec for batchv1::Job {
    type Spec = batchv1::JobSpec;

    fn spec_mut(&mut self) -> &mut Self::Spec {
        self.spec.get_or_insert_default()
    }
}
