use headscale_client::Status;

#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("headscale gRPC error: {0}")]
    HeadscaleApi(#[from] Status),
    #[error("object has no namespace")]
    MissingNamespace,
    #[error("object has no name")]
    UnnamedObject,
    #[error("required annotation '{0}' is missing")]
    MissingAnnotation(&'static str),
    #[error("invalid annotations: {0}")]
    InvalidAnnotations(&'static str),
    #[error("invalid annotation '{0}': {1}")]
    InvalidAnnotation(&'static str, String),
    #[error("WireGuard NodePort service has no nodePort assigned yet")]
    NodePortNotAssigned,
}
