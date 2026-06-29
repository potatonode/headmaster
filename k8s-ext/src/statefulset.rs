use super::*;

pub trait StatefulSetExt: ResourceBuilder {
    fn new(name: impl ToString) -> Self;
    fn replicas(self, replicas: i32) -> Self;
    fn service_name(self, service_name: impl ToString) -> Self;
    fn template(self, template: corev1::PodTemplateSpec) -> Self;
    fn volume_claim_templates(
        self,
        templates: impl IntoIterator<Item = corev1::PersistentVolumeClaim>,
    ) -> Self;
}

impl StatefulSetExt for appsv1::StatefulSet {
    fn new(name: impl ToString) -> Self {
        Self {
            metadata: make_metadata(name),
            ..Default::default()
        }
    }

    fn replicas(mut self, replicas: i32) -> Self {
        self.spec_mut().replicas.replace(replicas);
        self
    }

    fn service_name(mut self, service_name: impl ToString) -> Self {
        self.spec_mut()
            .service_name
            .replace(service_name.to_string());
        self
    }

    fn template(mut self, template: corev1::PodTemplateSpec) -> Self {
        self.spec_mut().template = template;
        self
    }

    fn volume_claim_templates(
        mut self,
        templates: impl IntoIterator<Item = corev1::PersistentVolumeClaim>,
    ) -> Self {
        self.spec_mut()
            .volume_claim_templates
            .replace(templates.into_iter().collect());
        self
    }
}

impl HasSpec for appsv1::StatefulSet {
    type Spec = appsv1::StatefulSetSpec;

    fn spec_mut(&mut self) -> &mut Self::Spec {
        self.spec.get_or_insert_default()
    }
}
