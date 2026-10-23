//! Everything that talks to Postgres.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

pub type Headers = Vec<(String, String)>;

pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

pub struct NewDelivery<'a> {
    pub source: &'a str,
    pub event_id: Option<&'a str>,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stored {
    Created(Uuid),
    /// The same provider event id was stored before. Nothing was written.
    Duplicate,
}

/// Stores a verified delivery and one pending job per target, in one statement
/// so either both exist or neither does.
///
/// `targets` is `(name, url)`. A repeated `(source, event_id)` hits the unique
/// index, the delivery CTE returns no row, and so no jobs are inserted.
pub async fn insert_verified(
    pool: &PgPool,
    d: &NewDelivery<'_>,
    targets: &[(String, String)],
) -> Result<Stored, sqlx::Error> {
    let names: Vec<&str> = targets.iter().map(|t| t.0.as_str()).collect();
    let urls: Vec<&str> = targets.iter().map(|t| t.1.as_str()).collect();

    let row: Option<(Uuid,)> = sqlx::query_as(
        "with d as (
           insert into deliveries (source, event_id, verification, headers, body)
           values ($1, $2, 'verified', $3, $4)
           on conflict (source, event_id) where event_id is not null do nothing
           returning id
         )
         insert into jobs (delivery_id, target, url)
         select d.id, t.name, t.url
         from d, unnest($5::text[], $6::text[]) as t(name, url)
         returning delivery_id",
    )
    .bind(d.source)
    .bind(d.event_id)
    .bind(sqlx::types::Json(d.headers))
    .bind(d.body)
    .bind(&names)
    .bind(&urls)
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        Some((id,)) => Stored::Created(id),
        None => Stored::Duplicate,
    })
}

/// Stores a delivery that failed verification, then drops the oldest rejected
/// rows beyond `keep`. Rejected deliveries get no event id, so a flood of bad
/// requests can never collide with a real event.
pub async fn insert_rejected(
    pool: &PgPool,
    d: &NewDelivery<'_>,
    reason: &str,
    keep: i64,
) -> Result<Option<Uuid>, sqlx::Error> {
    if keep == 0 {
        return Ok(None);
    }
    let (id,): (Uuid,) = sqlx::query_as(
        "insert into deliveries (source, verification, reject_reason, headers, body)
         values ($1, 'rejected', $2, $3, $4)
         returning id",
    )
    .bind(d.source)
    .bind(reason)
    .bind(sqlx::types::Json(d.headers))
    .bind(d.body)
    .fetch_one(pool)
    .await?;

    trim_rejected(pool, keep).await?;
    Ok(Some(id))
}

/// Keeps the newest `keep` rejected deliveries. `skip locked` lets concurrent
/// trims pass each other instead of deadlocking on the same rows.
pub async fn trim_rejected(pool: &PgPool, keep: i64) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "delete from deliveries where id in (
           select id from deliveries
           where verification = 'rejected'
           order by received_at desc, id
           offset $1
           for update skip locked
         )",
    )
    .bind(keep)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Jobs per status. All four statuses are always present, so a gauge drops
/// back to 0 instead of keeping its last value.
pub async fn job_counts(pool: &PgPool) -> Result<Vec<(&'static str, i64)>, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("select status, count(*) from jobs group by status")
            .fetch_all(pool)
            .await?;
    Ok(["pending", "in_flight", "done", "dead"]
        .into_iter()
        .map(|s| (s, rows.iter().find(|r| r.0 == s).map_or(0, |r| r.1)))
        .collect())
}

/// Deletes deliveries older than `older_than`, with their jobs and attempts.
/// A delivery that still has a job waiting or in flight is left alone, so
/// pruning never throws away work that has not been done yet. With `dry_run`
/// it only counts.
pub async fn prune(pool: &PgPool, older_than: Duration, dry_run: bool) -> Result<u64, sqlx::Error> {
    let secs = older_than.as_secs_f64();
    if dry_run {
        let n: i64 = sqlx::query_scalar(
            "select count(*) from deliveries d
             where d.received_at < now() - make_interval(secs => $1)
               and not exists (
                 select 1 from jobs j
                 where j.delivery_id = d.id and j.status in ('pending', 'in_flight'))",
        )
        .bind(secs)
        .fetch_one(pool)
        .await?;
        return Ok(n as u64);
    }
    let res = sqlx::query(
        "delete from deliveries d
         where d.received_at < now() - make_interval(secs => $1)
           and not exists (
             select 1 from jobs j
             where j.delivery_id = d.id and j.status in ('pending', 'in_flight'))",
    )
    .bind(secs)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
