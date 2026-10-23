//! `POST /in/{source}`: verify, store, answer fast.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::app::AppState;
use crate::store::{self, Headers, NewDelivery, Stored};

/// Headers that carry credentials. They are not stored, shown or forwarded.
const SENSITIVE: [&str; 4] = [
    "authorization",
    "cookie",
    "proxy-authorization",
    "x-callback-token",
];

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Headers as `(name, value)` pairs in the order received, minus credentials.
pub fn header_pairs(headers: &HeaderMap) -> Headers {
    headers
        .iter()
        .filter(|(name, _)| !SENSITIVE.contains(&name.as_str()))
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

pub async fn receive(
    State(st): State<AppState>,
    Path(source_name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let Some(source) = st.config.source(&source_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown source"})),
        )
            .into_response();
    };
    let label = source.name.clone();
    let outcome = |result: &'static str| {
        metrics::counter!("inbox_received_total", "source" => label.clone(), "result" => result)
            .increment(1);
        metrics::histogram!("inbox_receive_seconds", "source" => label.clone())
            .record(started.elapsed().as_secs_f64());
    };

    let stored_headers = header_pairs(&headers);

    if let Err(reason) = source.verifier.verify(&headers, &body, unix_now()) {
        let new = NewDelivery {
            source: &source.name,
            event_id: None,
            headers: &stored_headers,
            body: &body,
        };
        if let Err(e) =
            store::insert_rejected(&st.pool, &new, &reason.to_string(), st.config.rejected_keep)
                .await
        {
            // The caller still gets a 401. Failing to keep a debugging copy is not
            // worth turning a rejection into a 500.
            tracing::error!(source = %source.name, error = %e, "could not store rejected delivery");
        }
        tracing::warn!(source = %source.name, %reason, "rejected delivery");
        outcome("rejected");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "signature check failed"})),
        )
            .into_response();
    }

    let event_id = source
        .event_id
        .as_ref()
        .and_then(|e| e.extract(&headers, &body));
    if source.event_id.is_some() && event_id.is_none() {
        tracing::warn!(source = %source.name, "no event id found, delivery is not deduplicated");
    }

    let new = NewDelivery {
        source: &source.name,
        event_id: event_id.as_deref(),
        headers: &stored_headers,
        body: &body,
    };
    let targets: Vec<(String, String)> = source
        .targets
        .iter()
        .map(|t| (t.name.clone(), t.url.clone()))
        .collect();

    match store::insert_verified(&st.pool, &new, &targets).await {
        Ok(Stored::Created(id)) => {
            st.wake.notify_waiters();
            outcome("accepted");
            let mut res = (
                StatusCode::OK,
                Json(json!({"status": "accepted", "id": id})),
            )
                .into_response();
            if let Ok(v) = HeaderValue::from_str(&id.to_string()) {
                res.headers_mut().insert("x-inbox-delivery-id", v);
            }
            res
        }
        Ok(Stored::Duplicate) => {
            outcome("duplicate");
            let mut res = (StatusCode::OK, Json(json!({"status": "duplicate"}))).into_response();
            res.headers_mut()
                .insert("x-inbox-duplicate", HeaderValue::from_static("true"));
            res
        }
        Err(e) => {
            // 5xx makes the provider retry, which is what we want if we could not store it.
            tracing::error!(source = %source.name, error = %e, "could not store delivery");
            outcome("error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "could not store delivery"})),
            )
                .into_response()
        }
    }
}
