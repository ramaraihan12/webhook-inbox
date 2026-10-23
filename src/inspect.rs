//! Read-side queries for the UI: lists, one delivery's history, dead jobs.
//!
//! A delivery's status comes from its newest job per target, so a dead job
//! that was replayed successfully no longer makes the delivery look broken.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::store::Headers;

#[derive(Debug, Default, Clone)]
pub struct ListFilter {
    pub source: Option<String>,
    /// One of `pending`, `done`, `dead`, `rejected`.
    pub status: Option<String>,
}

#[derive(Debug, FromRow)]
pub struct DeliveryRow {
    pub id: Uuid,
    pub source: String,
    pub received_at: DateTime<Utc>,
    pub event_id: Option<String>,
    pub reject_reason: Option<String>,
    pub status: String,
    pub attempts: i64,
}

pub async fn list_deliveries(
    pool: &PgPool,
    filter: &ListFilter,
    limit: i64,
    offset: i64,
) -> Result<Vec<DeliveryRow>, sqlx::Error> {
    sqlx::query_as(
        "select * from (
           select d.id, d.source, d.received_at, d.event_id, d.reject_reason,
                  case when d.verification = 'rejected' then 'rejected'
                       when j.dead > 0 then 'dead'
                       when j.open > 0 then 'pending'
                       else 'done' end as status,
                  (select count(*) from attempts a where a.delivery_id = d.id) as attempts
           from deliveries d
           left join lateral (
             select count(*) filter (where cur.status = 'dead') as dead,
                    count(*) filter (where cur.status in ('pending', 'in_flight')) as open
             from (select distinct on (target) status
                   from jobs where delivery_id = d.id
                   order by target, created_at desc, id desc) cur
           ) j on true
           where ($1::text is null or d.source = $1)
         ) x
         where ($2::text is null or x.status = $2)
         order by x.received_at desc, x.id desc
         limit $3 offset $4",
    )
    .bind(&filter.source)
    .bind(&filter.status)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

#[derive(Debug)]
pub struct DeliveryDetail {
    pub id: Uuid,
    pub source: String,
    pub received_at: DateTime<Utc>,
    pub event_id: Option<String>,
    pub verification: String,
    pub reject_reason: Option<String>,
    pub headers: Headers,
    pub body: Vec<u8>,
}

pub async fn get_delivery(pool: &PgPool, id: Uuid) -> Result<Option<DeliveryDetail>, sqlx::Error> {
    type Row = (
        Uuid,
        String,
        DateTime<Utc>,
        Option<String>,
        String,
        Option<String>,
        serde_json::Value,
        Vec<u8>,
    );
    let row: Option<Row> = sqlx::query_as(
        "select id, source, received_at, event_id, verification, reject_reason, headers, body
         from deliveries where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(id, source, received_at, event_id, verification, reject_reason, headers, body)| {
            DeliveryDetail {
                id,
                source,
                received_at,
                event_id,
                verification,
                reject_reason,
                headers: serde_json::from_value(headers).unwrap_or_default(),
                body,
            }
        },
    ))
}

#[derive(Debug, FromRow)]
pub struct JobRow {
    pub id: Uuid,
    pub target: String,
    pub url: String,
    pub status: String,
    pub attempts: i32,
    pub replay: bool,
    pub next_attempt_at: DateTime<Utc>,
    pub last_status: Option<i32>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub async fn jobs_for(pool: &PgPool, delivery_id: Uuid) -> Result<Vec<JobRow>, sqlx::Error> {
    sqlx::query_as(
        "select id, target, url, status, attempts, replay, next_attempt_at,
                last_status, last_error, created_at
         from jobs where delivery_id = $1
         order by created_at, id",
    )
    .bind(delivery_id)
    .fetch_all(pool)
    .await
}

#[derive(Debug, FromRow)]
pub struct AttemptRow {
    pub target: String,
    pub replay: bool,
    pub attempt: i32,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i32,
    pub http_status: Option<i32>,
    pub error: Option<String>,
    pub outcome: String,
    pub retry_in_ms: Option<i64>,
}

pub async fn attempts_for(
    pool: &PgPool,
    delivery_id: Uuid,
) -> Result<Vec<AttemptRow>, sqlx::Error> {
    sqlx::query_as(
        "select j.target, j.replay, a.attempt, a.started_at, a.duration_ms,
                a.http_status, a.error, a.outcome, a.retry_in_ms
         from attempts a join jobs j on j.id = a.job_id
         where a.delivery_id = $1
         order by a.started_at, a.attempt",
    )
    .bind(delivery_id)
    .fetch_all(pool)
    .await
}

#[derive(Debug, FromRow)]
pub struct DeadRow {
    pub job_id: Uuid,
    pub delivery_id: Uuid,
    pub source: String,
    pub target: String,
    pub url: String,
    pub attempts: i32,
    pub last_status: Option<i32>,
    pub last_error: Option<String>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Dead jobs that nobody has retried since: a later job for the same delivery
/// and target (a replay) takes a job off this list.
pub async fn dead_jobs(pool: &PgPool, limit: i64) -> Result<Vec<DeadRow>, sqlx::Error> {
    sqlx::query_as(
        "select j.id as job_id, j.delivery_id, d.source, j.target, j.url, j.attempts,
                j.last_status, j.last_error, j.finished_at
         from jobs j
         join deliveries d on d.id = j.delivery_id
         where j.status = 'dead'
           and not exists (
             select 1 from jobs n
             where n.delivery_id = j.delivery_id and n.target = j.target
               and (n.created_at, n.id) > (j.created_at, j.id))
         order by j.finished_at desc nulls last, j.id
         limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// The delivery and target name behind a job, for the dead-letter Replay button.
pub async fn job_origin(
    pool: &PgPool,
    job_id: Uuid,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    sqlx::query_as("select delivery_id, target from jobs where id = $1")
        .bind(job_id)
        .fetch_optional(pool)
        .await
}
