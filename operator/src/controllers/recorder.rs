use k8s_openapi::api::core::v1::ObjectReference;
use kube::runtime::events::{Event, EventType, Recorder};

use crate::types::ResourceStatus;

#[allow(async_fn_in_trait)]
pub trait RecorderExt {
    async fn publish_created(&self, obj_ref: &ObjectReference) -> Result<(), kube::Error>;
    async fn publish_ready(&self, obj_ref: &ObjectReference) -> Result<(), kube::Error>;
    async fn publish_deleted(&self, obj_ref: &ObjectReference) -> Result<(), kube::Error>;
    async fn publish_warning(
        &self,
        obj_ref: &ObjectReference,
        reason: &str,
        note: &str,
    ) -> Result<(), kube::Error>;

    async fn publish_transitions<S: ResourceStatus>(
        &self,
        old_status: &S,
        new_status: &S,
        obj_ref: &ObjectReference,
    ) {
        if old_status.is_new()
            && let Err(e) = self.publish_created(obj_ref).await
        {
            tracing::trace!(error = %e, "failed to publish Created event");
        }
        if !old_status.is_ready()
            && new_status.is_ready()
            && let Err(e) = self.publish_ready(obj_ref).await
        {
            tracing::trace!(error = %e, "failed to publish Ready event");
        }
    }
}

impl RecorderExt for Recorder {
    async fn publish_created(&self, obj_ref: &ObjectReference) -> Result<(), kube::Error> {
        self.publish(&lifecycle_event(obj_ref, "Created", "created"), obj_ref)
            .await
    }

    async fn publish_ready(&self, obj_ref: &ObjectReference) -> Result<(), kube::Error> {
        self.publish(&lifecycle_event(obj_ref, "Ready", "is ready"), obj_ref)
            .await
    }

    async fn publish_deleted(&self, obj_ref: &ObjectReference) -> Result<(), kube::Error> {
        self.publish(&lifecycle_event(obj_ref, "Deleted", "deleted"), obj_ref)
            .await
    }

    async fn publish_warning(
        &self,
        obj_ref: &ObjectReference,
        reason: &str,
        note: &str,
    ) -> Result<(), kube::Error> {
        self.publish(
            &Event {
                type_: EventType::Warning,
                reason: reason.to_string(),
                note: Some(note.to_string()),
                action: reason.to_string(),
                secondary: None,
            },
            obj_ref,
        )
        .await
    }
}

fn lifecycle_event(obj_ref: &ObjectReference, reason: &str, verb: &str) -> Event {
    let note = format!(
        "{} {} {verb}",
        obj_ref.kind.as_deref().unwrap_or("resource"),
        obj_ref.name.as_deref().unwrap_or("unknown"),
    );
    Event {
        type_: EventType::Normal,
        reason: reason.to_string(),
        note: Some(note),
        action: reason.to_string(),
        secondary: None,
    }
}
