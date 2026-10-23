//! A tiny target for the demo and for measuring delivery throughput.
//!
//! * `POST /ok` (or any other path): 200.
//! * `POST /flaky`: 500 until `x-inbox-attempt` reaches 3, then 200.
//! * `POST /down`: always 503 with `Retry-After: 1`.
//! * `GET /count`: how many POSTs it has seen, as plain text.
//!
//! One log line per request unless `SINK_QUIET=1`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

#[derive(Clone)]
struct Sink {
    seen: Arc<AtomicU64>,
    quiet: bool,
}

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8021".to_string());
    let sink = Sink {
        seen: Arc::new(AtomicU64::new(0)),
        quiet: std::env::var("SINK_QUIET").is_ok_and(|v| v == "1"),
    };
    let app = Router::new()
        .route("/count", get(count))
        .route("/{*path}", post(receive))
        .with_state(sink);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("port is free");
    println!("echo-sink listening on :{port}");
    axum::serve(listener, app).await.expect("server runs");
}

async fn count(State(sink): State<Sink>) -> String {
    sink.seen.load(Ordering::Relaxed).to_string()
}

async fn receive(
    State(sink): State<Sink>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sink.seen.fetch_add(1, Ordering::Relaxed);
    let attempt: u32 = headers
        .get("x-inbox-attempt")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let response = match path.as_str() {
        "flaky" if attempt < 3 => (StatusCode::INTERNAL_SERVER_ERROR, "not yet").into_response(),
        "down" => {
            let mut r = (StatusCode::SERVICE_UNAVAILABLE, "down for maintenance").into_response();
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            r
        }
        _ => (StatusCode::OK, "ok").into_response(),
    };

    if !sink.quiet {
        let delivery = headers
            .get("x-inbox-delivery-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");
        println!(
            "POST /{path} delivery={delivery} attempt={attempt} bytes={} -> {}",
            body.len(),
            response.status().as_u16()
        );
    }
    response
}
