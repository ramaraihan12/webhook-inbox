//! The HTTP side: shared state and the router.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{get, post};
use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;
use tokio::sync::Notify;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::verify::Secret;
use crate::{ops, receive, ui};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    /// Woken when a delivery is stored, so idle workers do not wait for the
    /// next poll.
    pub wake: Arc<Notify>,
    pub metrics: PrometheusHandle,
    /// Token for the UI. With none set the UI answers 503.
    pub admin_token: Option<Arc<Secret>>,
}

pub fn router(state: AppState) -> Router {
    let max_body = state.config.server.max_body_bytes;
    let timeout = state.config.server.request_timeout;

    let receive_routes = Router::new()
        .route("/in/{source}", post(receive::receive))
        // The tower-http layer enforces the limit, including while streaming.
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(max_body))
        // A request span (method, uri) and a line with status and latency, both
        // at debug level: RUST_LOG=info,tower_http=debug turns them on.
        .layer(TraceLayer::new_for_http());

    Router::new()
        .merge(receive_routes)
        .merge(ui::routes(state.clone()))
        .route("/healthz", get(ops::healthz))
        .route("/metrics", get(ops::metrics))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout,
        ))
        .with_state(state)
}

/// How long shutdown waits for in-flight work before giving up.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
