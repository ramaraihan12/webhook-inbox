//! A per-client request limit for the routes that write to the database. Off
//! unless `[server.rate_limit]` is in the config.
//!
//! Counts live in memory, so they are per instance and a restart forgets them.
//! That is enough to keep a public demo usable, it is not a quota. Behind a
//! proxy every socket comes from the proxy, so the caller's address is read from
//! a header the proxy sets. Requests without a usable address share one count.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::config::RateLimitConfig;

const WINDOW: Duration = Duration::from_secs(60);
/// Most callers tracked at once, so the table cannot grow without bound.
const MAX_CLIENTS: usize = 10_000;

/// What the routes hold. `None` when no limit is configured.
pub type Limiter = Option<Arc<RateLimiter>>;

struct Window {
    started: Instant,
    count: u32,
}

pub struct RateLimiter {
    per_minute: u32,
    client_ip_header: HeaderName,
    clients: Mutex<HashMap<Option<IpAddr>, Window>>,
}

impl RateLimiter {
    pub fn new(config: &RateLimitConfig) -> RateLimiter {
        RateLimiter {
            per_minute: config.per_minute,
            client_ip_header: config.client_ip_header.clone(),
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn client(&self, headers: &HeaderMap) -> Option<IpAddr> {
        headers
            .get(&self.client_ip_header)?
            .to_str()
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Counts one request and says if it may go on. With the table full of
    /// callers still inside their window, a new caller is refused.
    fn allow(&self, client: Option<IpAddr>, now: Instant) -> bool {
        let mut clients = self.clients.lock().expect("no panics while locked");
        if clients.len() >= MAX_CLIENTS && !clients.contains_key(&client) {
            clients.retain(|_, w| now.duration_since(w.started) < WINDOW);
            if clients.len() >= MAX_CLIENTS {
                return false;
            }
        }
        let window = clients.entry(client).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(window.started) >= WINDOW {
            window.started = now;
            window.count = 0;
        }
        window.count = window.count.saturating_add(1);
        window.count <= self.per_minute
    }
}

/// Answers 429 before the handler runs, so a refused request is never read or
/// stored.
pub async fn check(State(limiter): State<Limiter>, req: Request, next: Next) -> Response {
    if let Some(limiter) = limiter
        && !limiter.allow(limiter.client(req.headers()), Instant::now())
    {
        metrics::counter!("inbox_rate_limited_total").increment(1);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            Json(json!({"error": "too many requests"})),
        )
            .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(per_minute: u32) -> RateLimiter {
        RateLimiter::new(&RateLimitConfig {
            per_minute,
            client_ip_header: HeaderName::from_static("cf-connecting-ip"),
        })
    }

    fn ip(n: u32) -> Option<IpAddr> {
        Some(IpAddr::from(n.to_be_bytes()))
    }

    #[test]
    fn a_client_gets_its_limit_and_then_is_refused() {
        let l = limiter(2);
        let now = Instant::now();
        assert!(l.allow(ip(1), now));
        assert!(l.allow(ip(1), now));
        assert!(!l.allow(ip(1), now));
    }

    #[test]
    fn clients_are_counted_separately() {
        let l = limiter(1);
        let now = Instant::now();
        assert!(l.allow(ip(1), now));
        assert!(!l.allow(ip(1), now));
        assert!(l.allow(ip(2), now));
    }

    #[test]
    fn the_count_starts_over_after_the_window() {
        let l = limiter(1);
        let now = Instant::now();
        assert!(l.allow(ip(1), now));
        assert!(!l.allow(ip(1), now + Duration::from_secs(59)));
        assert!(l.allow(ip(1), now + WINDOW));
    }

    #[test]
    fn a_full_table_refuses_newcomers_until_old_windows_expire() {
        let l = limiter(5);
        let now = Instant::now();
        for n in 0..MAX_CLIENTS as u32 {
            assert!(l.allow(ip(n), now));
        }
        let newcomer = ip(MAX_CLIENTS as u32);
        assert!(!l.allow(newcomer, now));
        // Someone already in the table is not affected.
        assert!(l.allow(ip(0), now));

        assert!(l.allow(newcomer, now + WINDOW));
        assert_eq!(l.clients.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_address_comes_from_the_configured_header() {
        let l = limiter(1);
        let mut headers = HeaderMap::new();
        assert_eq!(l.client(&headers), None);

        headers.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
        assert_eq!(l.client(&headers), Some("203.0.113.7".parse().unwrap()));

        headers.insert("cf-connecting-ip", "2001:db8::1".parse().unwrap());
        assert_eq!(l.client(&headers), Some("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn a_value_that_is_not_an_address_counts_as_no_address() {
        let l = limiter(1);
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", "not-an-ip".parse().unwrap());
        assert_eq!(l.client(&headers), None);
    }
}
