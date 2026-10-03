//! Concurrency and timeout limit layer, exercised on a deliberately slow route.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use pair_api::limits::{apply, Limits};
use tower::ServiceExt;

const SLOW: Duration = Duration::from_millis(300);
const PERMIT_TAKEN_GRACE: Duration = Duration::from_millis(50);

async fn slow_handler() -> &'static str {
    tokio::time::sleep(SLOW).await;
    "done"
}

async fn fast_handler() -> &'static str {
    "ok"
}

fn slow_app(limits: Limits) -> Router {
    apply(
        Router::new()
            .route("/slow", get(slow_handler))
            .route("/fast", get(fast_handler)),
        limits,
    )
}

async fn status_of(app: Router, uri: &str) -> StatusCode {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    app.oneshot(req).await.expect("infallible").status()
}

#[tokio::test]
async fn limits_shed_excess_concurrent_requests_with_503() {
    let app = slow_app(Limits {
        request_timeout: Duration::from_secs(5),
        max_in_flight: 1,
    });
    let first = tokio::spawn(status_of(app.clone(), "/slow"));
    tokio::time::sleep(PERMIT_TAKEN_GRACE).await; // let the first request take the permit
    assert_eq!(
        status_of(app.clone(), "/fast").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(first.await.expect("join"), StatusCode::OK);
    // Permit released: capacity is back.
    assert_eq!(status_of(app, "/fast").await, StatusCode::OK);
}

#[tokio::test]
async fn limits_time_out_slow_handlers_with_504() {
    let app = slow_app(Limits {
        request_timeout: Duration::from_millis(50),
        max_in_flight: 4,
    });
    assert_eq!(
        status_of(app.clone(), "/slow").await,
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(status_of(app, "/fast").await, StatusCode::OK);
}
