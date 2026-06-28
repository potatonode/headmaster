use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;

pub fn router(ready: Arc<AtomicBool>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(ready)
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn readyz(State(ready): State<Arc<AtomicBool>>) -> StatusCode {
    if ready.load(Ordering::Relaxed) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn healthz_always_returns_200() {
        assert_eq!(healthz().await, StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_reflects_readiness_flag() {
        let ready = Arc::new(AtomicBool::new(false));
        assert_eq!(
            readyz(State(ready.clone())).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        ready.store(true, Ordering::Relaxed);
        assert_eq!(readyz(State(ready)).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let response = router(Arc::new(AtomicBool::new(false)))
            .oneshot(Request::get("/unknown").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
