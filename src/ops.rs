//! `/healthz` and `/metrics`.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

use crate::app::AppState;
use crate::store;

pub async fn healthz(State(st): State<AppState>) -> impl IntoResponse {
    match sqlx::query("select 1").execute(&st.pool).await {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(e) => {
            tracing::error!(error = %e, "health check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable")
        }
    }
}

pub async fn metrics(State(st): State<AppState>) -> impl IntoResponse {
    // Queue depth is read from the table at scrape time, so it is right even
    // after a restart and across several instances.
    match store::job_counts(&st.pool).await {
        Ok(counts) => {
            for (status, n) in counts {
                metrics::gauge!("inbox_jobs", "status" => status).set(n as f64);
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not read queue depth for /metrics"),
    }
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        st.metrics.render(),
    )
}
