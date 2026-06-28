use std::sync::{Arc, Mutex};

/// Signature for a fault-injection responder: given the HTTP method and request
/// path, return `(status_code, body_bytes)`.
pub type Respond = fn(&http::Method, &str) -> (u16, Vec<u8>);

/// Recorded request: `(METHOD, path)`.
pub type Calls = Arc<Mutex<Vec<(String, String)>>>;

/// Tower service wired into [`kube::Client::new`] to simulate Kubernetes API
/// server responses without a real cluster. All requests are recorded before
/// the responder runs, so tests can assert on which K8s calls were issued.
pub struct FaultService {
    respond: Respond,
    calls: Calls,
}

impl FaultService {
    /// Returns a [`kube::Client`] backed by this service and a handle to the
    /// recorded call log.
    pub fn tracked(respond: Respond) -> (kube::Client, Calls) {
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let svc = Self {
            respond,
            calls: Arc::clone(&calls),
        };
        (kube::Client::new(svc, "default"), calls)
    }

    /// Like [`tracked`] but discards the call log. Convenient when the test
    /// only cares about the return value, not which K8s requests were issued.
    pub fn client(respond: Respond) -> kube::Client {
        Self::tracked(respond).0
    }
}

impl tower::Service<http::Request<kube::client::Body>> for FaultService {
    type Response = http::Response<kube::client::Body>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), path.clone()));
        let (code, body) = (self.respond)(&method, &path);
        let response = http::Response::builder()
            .status(code)
            .body(kube::client::Body::from(body))
            .unwrap();
        std::future::ready(Ok(response))
    }
}

// ── common responders ─────────────────────────────────────────────────────────

pub fn all_404(_: &http::Method, _: &str) -> (u16, Vec<u8>) {
    (404, br#"{"code":404}"#.to_vec())
}

pub fn all_500(_: &http::Method, _: &str) -> (u16, Vec<u8>) {
    (500, br#"{"code":500}"#.to_vec())
}
