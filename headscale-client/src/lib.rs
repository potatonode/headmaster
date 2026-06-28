pub mod policy;

pub mod headscale {
    pub mod v1 {
        tonic::include_proto!("headscale.v1");
    }
}

#[cfg(feature = "fake-server")]
pub mod fake;

pub use headscale::v1::headscale_service_client::HeadscaleServiceClient;
pub use tonic::Code;
pub use tonic::Status;
pub use tonic::service::interceptor::InterceptedService;
pub use tonic::transport::Channel;
pub use tonic::transport::Error as TransportError;

/// gRPC interceptor that injects an `Authorization: Bearer <token>` header.
#[derive(Clone)]
pub struct AuthInterceptor {
    token: tonic::metadata::MetadataValue<tonic::metadata::Ascii>,
}

impl AuthInterceptor {
    pub fn bearer(api_key: &str) -> Self {
        Self {
            token: format!("Bearer {api_key}")
                .parse()
                .expect("api key must be valid ASCII"),
        }
    }
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        req.metadata_mut()
            .insert("authorization", self.token.clone());
        Ok(req)
    }
}

pub type AuthenticatedClient = HeadscaleServiceClient<InterceptedService<Channel, AuthInterceptor>>;

#[async_trait::async_trait]
pub trait HeadscaleConnector: Send + Sync {
    async fn connect(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<AuthenticatedClient, TransportError>;
}

pub struct LiveConnector;

#[async_trait::async_trait]
impl HeadscaleConnector for LiveConnector {
    async fn connect(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<AuthenticatedClient, TransportError> {
        let channel = Channel::from_shared(endpoint.to_owned())
            .expect("endpoint is valid URI")
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await?;
        Ok(HeadscaleServiceClient::with_interceptor(
            channel,
            AuthInterceptor::bearer(api_key),
        ))
    }
}
