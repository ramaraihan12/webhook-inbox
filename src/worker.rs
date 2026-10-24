//! Delivery workers: claim jobs, POST the original body, record the result.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use futures_util::future::join_all;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use sqlx::PgPool;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::backoff;
use crate::config::{Config, Source, Target};
use crate::queue::{self, AttemptResult, Claimed, Verdict};

/// Request headers that must not be copied from the original delivery: they
/// describe the hop to us, not the body, and the HTTP client sets its own.
const NOT_FORWARDED: [&str; 12] = [
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "accept-encoding",
    "proxy-connection",
    "expect",
    "user-agent",
];

/// How much of an error response is kept to explain a failure.
const SNIPPET_BYTES: usize = 300;

#[derive(Clone)]
pub struct Worker {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub wake: Arc<Notify>,
    client: reqwest::Client,
}

impl Worker {
    pub fn new(pool: PgPool, config: Arc<Config>, wake: Arc<Notify>) -> Worker {
        let client = reqwest::Client::builder()
            .user_agent(concat!("webhook-inbox/", env!("CARGO_PKG_VERSION")))
            // A redirect on a POST is a misconfigured target, not something to follow.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("http client builds with default tls");
        Worker {
            pool,
            config,
            wake,
            client,
        }
    }

    /// Runs `worker.count` claim loops until `shutdown` is cancelled, then waits
    /// for deliveries already started to finish. Nothing new is claimed once
    /// shutdown begins.
    pub async fn run(&self, shutdown: CancellationToken) {
        let loops = (0..self.config.worker.count).map(|id| {
            let worker = self.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { worker.claim_loop(id, shutdown).await })
        });
        for handle in join_all(loops).await {
            if let Err(e) = handle {
                tracing::error!(error = %e, "worker task failed");
            }
        }
    }

    async fn claim_loop(&self, id: usize, shutdown: CancellationToken) {
        let cfg = &self.config.worker;
        let slots = cfg.batch as u32;
        // One slot per job this loop may have in flight. A slow target only
        // holds its own slots, the loop keeps claiming with the free ones.
        let sem = Arc::new(Semaphore::new(slots as usize));
        while !shutdown.is_cancelled() {
            let Some(first) = self.next_slot(&sem, &shutdown).await else {
                break;
            };
            let mut permits = vec![first];
            while permits.len() < slots as usize {
                match sem.clone().try_acquire_owned() {
                    Ok(p) => permits.push(p),
                    Err(_) => break,
                }
            }
            let jobs = match queue::claim_jobs(&self.pool, permits.len() as i64, cfg.lease).await {
                Ok(jobs) => jobs,
                Err(e) => {
                    tracing::error!(worker = id, error = %e, "could not claim jobs");
                    drop(permits);
                    self.idle(&shutdown, cfg.poll_interval).await;
                    continue;
                }
            };
            if jobs.is_empty() {
                drop(permits);
                self.idle(&shutdown, cfg.poll_interval).await;
                continue;
            }
            // Not raced against `shutdown`: a claimed job is always finished.
            // Permits beyond the jobs found are released right here.
            for job in jobs {
                let permit = permits.pop().expect("one permit per claimed job");
                let worker = self.clone();
                tokio::spawn(async move {
                    worker.process(job).await;
                    drop(permit);
                });
            }
        }
        // Every slot back means every started delivery has finished.
        let _ = sem.acquire_many(slots).await;
        tracing::debug!(worker = id, "worker stopped");
    }

    /// Waits for a free slot, or returns `None` once shutdown begins.
    async fn next_slot(
        &self,
        sem: &Arc<Semaphore>,
        shutdown: &CancellationToken,
    ) -> Option<OwnedSemaphorePermit> {
        tokio::select! {
            _ = shutdown.cancelled() => None,
            p = sem.clone().acquire_owned() => p.ok(),
        }
    }

    async fn idle(&self, shutdown: &CancellationToken, poll: Duration) {
        tokio::select! {
            _ = shutdown.cancelled() => {}
            _ = self.wake.notified() => {}
            _ = tokio::time::sleep(poll) => {}
        }
    }

    async fn process(&self, job: Claimed) {
        let result = self.deliver(&job).await;

        let outcome = match &result.verdict {
            Verdict::Done => "ok",
            Verdict::Retry(_) => "retry",
            Verdict::Dead => "dead",
        };
        metrics::counter!("inbox_delivery_attempts_total", "source" => job.source.clone(), "outcome" => outcome)
            .increment(1);
        metrics::histogram!("inbox_delivery_seconds", "source" => job.source.clone())
            .record(result.elapsed.as_secs_f64());

        match queue::finish_job(&self.pool, &job, &result).await {
            Ok(true) => {
                tracing::info!(
                    delivery = %job.delivery_id, target = %job.target, attempt = job.attempt,
                    outcome, status = ?result.http_status, "delivery attempt"
                );
            }
            Ok(false) => tracing::warn!(
                job = %job.job_id, attempt = job.attempt,
                "lost the lease before recording this attempt, another worker has the job"
            ),
            Err(e) => tracing::error!(
                job = %job.job_id, error = %e,
                "could not record attempt, the job will be retried when its lease expires"
            ),
        }
    }

    /// Makes the HTTP call and decides what happens next. Never fails: every
    /// problem becomes a `Retry` or `Dead` verdict with the reason attached.
    async fn deliver(&self, job: &Claimed) -> AttemptResult {
        let started_at = Utc::now();
        let clock = Instant::now();
        let finish = |verdict, http_status, error| AttemptResult {
            verdict,
            http_status,
            error,
            started_at,
            duration_ms: clock.elapsed().as_millis().min(i32::MAX as u128) as i32,
            elapsed: clock.elapsed(),
        };

        let Some((source, target)) = self.config.source(&job.source).and_then(|s| {
            s.targets
                .iter()
                .find(|t| t.name == job.target)
                .map(|t| (s, t))
        }) else {
            return finish(
                Verdict::Dead,
                None,
                Some(format!(
                    "target {:?} is no longer in the config",
                    job.target
                )),
            );
        };

        let request = self
            .client
            .post(&job.url)
            .timeout(target.timeout)
            .headers(forward_headers(job, source, target))
            .body(job.body.clone());

        let (http_status, error, retry_after) = match request.send().await {
            Ok(resp) if resp.status().is_success() => {
                return finish(Verdict::Done, Some(i32::from(resp.status().as_u16())), None);
            }
            Ok(mut resp) => {
                let status = resp.status();
                let retry_after = resp
                    .headers()
                    .get(http::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| backoff::parse_retry_after(v, SystemTime::now()));
                let snippet = match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        let end = chunk.len().min(SNIPPET_BYTES);
                        String::from_utf8_lossy(&chunk[..end]).trim().to_string()
                    }
                    _ => String::new(),
                };
                (
                    Some(i32::from(status.as_u16())),
                    describe_status(status, &snippet),
                    retry_after,
                )
            }
            Err(e) => (None, describe_error(e), None),
        };

        let verdict = if job.attempt >= self.config.retry.max_attempts {
            Verdict::Dead
        } else {
            Verdict::Retry(backoff::next_delay(
                job.attempt as u32,
                &self.config.retry,
                retry_after,
                fastrand::f64(),
            ))
        };
        finish(verdict, http_status, Some(error))
    }
}

fn describe_status(status: StatusCode, snippet: &str) -> String {
    if snippet.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {}: {}", status.as_u16(), snippet)
    }
}

fn describe_error(e: reqwest::Error) -> String {
    // The URL comes from the config and may carry a token, so leave it out.
    let e = e.without_url();
    if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        format!("could not connect: {e}")
    } else {
        e.to_string()
    }
}

/// The original headers (minus hop-by-hop ones), then the target's own, then
/// the `x-inbox-*` headers. Later entries replace earlier ones.
fn forward_headers(job: &Claimed, source: &Source, target: &Target) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in &job.headers {
        if NOT_FORWARDED.contains(&name.as_str()) || name.starts_with("x-inbox-") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            out.append(name, value);
        }
    }
    for (name, value) in &target.headers {
        out.insert(name.clone(), value.clone());
    }
    out.insert(
        "x-inbox-delivery-id",
        HeaderValue::from_str(&job.delivery_id.to_string()).expect("uuid is ascii"),
    );
    out.insert("x-inbox-attempt", HeaderValue::from(job.attempt));
    // What the signature check covered, so the target knows when to confirm
    // with the provider before it acts (see "Confirm before you act" in the README).
    out.insert(
        "x-inbox-verified",
        HeaderValue::from_static(source.verifier.scope().tag),
    );
    out.insert(
        "x-inbox-source",
        HeaderValue::from_str(&job.source).expect("source names are ascii"),
    );
    out
}
