//! The job queue: claiming, finishing and replaying jobs.
//!
//! The queue is the `jobs` table. Several instances can run against it because
//! `claim_jobs` takes rows with `for update skip locked`: a row another worker
//! has locked is skipped, not waited for, so two workers never get the same job.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::store::Headers;

/// A job a worker now holds, with everything needed to make the HTTP call.
#[derive(Debug)]
pub struct Claimed {
    pub job_id: Uuid,
    pub delivery_id: Uuid,
    pub source: String,
    pub target: String,
    pub url: String,
    /// 1 for the first try. Includes tries lost to a crashed worker.
    pub attempt: i32,
    pub headers: Headers,
    pub body: Vec<u8>,
}

type ClaimRow = (
    Uuid,
    Uuid,
    String,
    String,
    String,
    i32,
    serde_json::Value,
    Vec<u8>,
);

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Done,
    /// Try again after this long.
    Retry(Duration),
    Dead,
}

#[derive(Debug, Clone)]
pub struct AttemptResult {
    pub verdict: Verdict,
    pub http_status: Option<i32>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i32,
    /// The same duration at full precision, for the latency histogram.
    pub elapsed: Duration,
}

/// Takes up to `limit` due jobs and locks them for `lease`.
///
/// Due means pending with `next_attempt_at` in the past, or in flight with an
/// expired lease (the worker holding it died). Claiming moves `next_attempt_at`
/// to the lease end, so both cases are one range condition on `next_attempt_at`
/// that the `jobs_due` index can stop at. `attempts` goes up by one here, so the
/// number a worker sees is the number of this try.
pub async fn claim_jobs(
    pool: &PgPool,
    limit: i64,
    lease: Duration,
) -> Result<Vec<Claimed>, sqlx::Error> {
    let rows: Vec<ClaimRow> = sqlx::query_as(
        "with picked as (
               select id from jobs
               where status in ('pending', 'in_flight') and next_attempt_at <= now()
               order by next_attempt_at
               limit $1
               for update skip locked
             ), claimed as (
               update jobs j
               set status = 'in_flight',
                   attempts = j.attempts + 1,
                   locked_until = now() + make_interval(secs => $2),
                   next_attempt_at = now() + make_interval(secs => $2)
               from picked
               where j.id = picked.id
               returning j.id, j.delivery_id, j.target, j.url, j.attempts
             )
             select c.id, c.delivery_id, d.source, c.target, c.url, c.attempts, d.headers, d.body
             from claimed c
             join deliveries d on d.id = c.delivery_id",
    )
    .bind(limit)
    .bind(lease.as_secs_f64())
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(job_id, delivery_id, source, target, url, attempt, headers, body)| Claimed {
                job_id,
                delivery_id,
                source,
                target,
                url,
                attempt,
                headers: serde_json::from_value(headers).unwrap_or_default(),
                body,
            },
        )
        .collect())
}

/// Records the result of a try and moves the job on, atomically.
///
/// The update only applies while this worker still owns the job (it is in
/// flight and `attempts` still equals this try). If the lease ran out and
/// someone else took the job over, nothing is written and `false` comes back,
/// so one try is never recorded twice.
pub async fn finish_job(
    pool: &PgPool,
    job: &Claimed,
    result: &AttemptResult,
) -> Result<bool, sqlx::Error> {
    let (status, delay, outcome) = match &result.verdict {
        Verdict::Done => ("done", None, "ok"),
        Verdict::Retry(d) => ("pending", Some(d.as_secs_f64()), "retry"),
        Verdict::Dead => ("dead", None, "dead"),
    };

    // One statement, so a try costs one round trip: the update only succeeds
    // while this worker still owns the job, and the attempt row is inserted
    // from whatever the update returned.
    let recorded = sqlx::query(
        "with moved as (
           update jobs
           set status = $3,
               next_attempt_at = case when $4::float8 is null then next_attempt_at
                                      else now() + make_interval(secs => $4::float8) end,
               locked_until = null,
               last_status = $5,
               last_error = $6,
               finished_at = case when $3 in ('done', 'dead') then now() end
           where id = $1 and status = 'in_flight' and attempts = $2
           returning id, delivery_id
         )
         insert into attempts
           (job_id, delivery_id, attempt, started_at, duration_ms, http_status, error, outcome, retry_in_ms)
         select id, delivery_id, $2, $7, $8, $5, $6, $9, $10 from moved",
    )
    .bind(job.job_id)
    .bind(job.attempt)
    .bind(status)
    .bind(delay)
    .bind(result.http_status)
    .bind(&result.error)
    .bind(result.started_at)
    .bind(result.duration_ms)
    .bind(outcome)
    .bind(delay.map(|s| (s * 1000.0) as i64))
    .execute(pool)
    .await?
    .rows_affected();

    Ok(recorded == 1)
}

/// How long until the next job is due, `None` with nothing waiting or in flight.
/// Claiming moves `next_attempt_at` to the lease end, so this also covers a job
/// held by a worker that may die. The wait comes from the database clock, the
/// same one `claim_jobs` compares with.
pub async fn next_due_in(pool: &PgPool) -> Result<Option<Duration>, sqlx::Error> {
    let secs: Option<f64> = sqlx::query_scalar(
        "select extract(epoch from min(next_attempt_at) - now())::float8
         from jobs
         where status in ('pending', 'in_flight')",
    )
    .fetch_one(pool)
    .await?;
    Ok(secs.map(|s| Duration::from_secs_f64(s.max(0.0))))
}

/// Queues a fresh job for every target of a verified delivery. The old jobs
/// and their attempts stay, so the timeline shows the replay as new tries.
/// Returns how many jobs were created, 0 if the delivery is unknown or was
/// rejected.
pub async fn replay_delivery(
    pool: &PgPool,
    delivery_id: Uuid,
    targets: &[(String, String)],
) -> Result<u64, sqlx::Error> {
    let names: Vec<&str> = targets.iter().map(|t| t.0.as_str()).collect();
    let urls: Vec<&str> = targets.iter().map(|t| t.1.as_str()).collect();
    let res = sqlx::query(
        "insert into jobs (delivery_id, target, url, replay)
         select d.id, t.name, t.url, true
         from deliveries d, unnest($2::text[], $3::text[]) as t(name, url)
         where d.id = $1 and d.verification = 'verified'",
    )
    .bind(delivery_id)
    .bind(&names)
    .bind(&urls)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
