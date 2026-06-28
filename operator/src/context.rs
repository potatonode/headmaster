use std::sync::Arc;

use headscale_client::HeadscaleConnector;
use kube::Client;
use kube::runtime::events::{Recorder, Reporter};

pub struct Context {
    pub client: Client,
    pub operator_namespace: String,
    pub headscale: Arc<dyn HeadscaleConnector>,
    pub reporter: Reporter,
    pub headscale_image: String,
    pub proxy_image: String,
    pub operator_image: String,
    /// Namespaces the Ingress controller watches. Empty means all namespaces.
    pub ingress_watch_namespaces: Vec<String>,
}

impl Context {
    pub fn recorder(&self) -> Recorder {
        Recorder::new(self.client.clone(), self.reporter.clone())
    }
}
