//! The admin UI under `/ui`: list, detail with an attempt timeline, dead
//! letters and replay. Server-rendered with askama, small bits of htmx.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::inspect::{self, AttemptRow, DeadRow, DeliveryDetail, DeliveryRow, JobRow, ListFilter};
use crate::queue;
use crate::verify::ct_eq;

const PAGE_SIZE: i64 = 50;
const STATUSES: [&str; 4] = ["pending", "done", "dead", "rejected"];
/// Bodies longer than this are cut in the page. The stored copy is whole.
const BODY_PREVIEW_BYTES: usize = 64 * 1024;

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/ui", get(list_page))
        .route("/ui/rows", get(rows_partial))
        .route("/ui/dead", get(dead_page))
        .route("/ui/deliveries/{id}", get(detail_page))
        .route("/ui/deliveries/{id}/activity", get(activity_partial))
        .route("/ui/deliveries/{id}/replay", post(replay_delivery))
        .route("/ui/jobs/{id}/replay", post(replay_job))
        .route("/ui/static/app.css", get(css))
        .route("/ui/static/htmx.min.js", get(htmx))
        .layer(middleware::from_fn_with_state(state, guard))
}

// ---- auth and headers ------------------------------------------------------

/// Checks the admin token (`Authorization: Bearer <token>`, or Basic auth with
/// the token as the password so a browser can prompt for it) and adds the
/// security headers every UI response gets.
async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let Some(token) = st.admin_token.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The UI is off because ADMIN_TOKEN is not set.",
        )
            .into_response();
    };
    if !token_matches(req.headers(), token.as_bytes()) {
        let mut res = (StatusCode::UNAUTHORIZED, "Admin token required.").into_response();
        res.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Basic realm="webhook-inbox", charset="UTF-8""#),
        );
        return res;
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
             img-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    res
}

fn token_matches(headers: &HeaderMap, token: &[u8]) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    if let Some(given) = value.strip_prefix("Bearer ") {
        return ct_eq(given.trim().as_bytes(), token);
    }
    if let Some(encoded) = value.strip_prefix("Basic ") {
        let Ok(decoded) = STANDARD.decode(encoded.trim()) else {
            return false;
        };
        // user:password, and only the password matters.
        let password = decoded.splitn(2, |b| *b == b':').nth(1).unwrap_or(&[]);
        return ct_eq(password, token);
    }
    false
}

/// A cross-site form post cannot set a custom header, so requiring htmx's own
/// header keeps the Replay buttons safe from CSRF even with Basic auth cached
/// in the browser.
fn require_htmx(headers: &HeaderMap) -> Result<(), Fail> {
    if headers.get("hx-request").is_some_and(|v| v == "true") {
        Ok(())
    } else {
        Err(Fail(Box::new(
            (
                StatusCode::BAD_REQUEST,
                "This action is only available from the UI.",
            )
                .into_response(),
        )))
    }
}

// ---- views -----------------------------------------------------------------

fn fmt_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn short(id: Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

fn fmt_ms(ms: i64) -> String {
    if ms == 0 {
        "under 1 ms".to_string()
    } else if ms >= 10_000 {
        format!("{:.0} s", ms as f64 / 1000.0)
    } else if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

fn render<T: Template>(t: &T) -> Response {
    match t.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not render the page.",
            )
                .into_response()
        }
    }
}

/// An error response that can be passed up with `?`. Boxed because a
/// `Response` is large for the `Err` side of a `Result`.
struct Fail(Box<Response>);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        *self.0
    }
}

fn db_fail(e: sqlx::Error) -> Fail {
    Fail(Box::new(db_error(e)))
}

fn db_error(e: sqlx::Error) -> Response {
    tracing::error!(error = %e, "ui query failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "Database error.").into_response()
}

struct RowView {
    id: Uuid,
    short: String,
    received: String,
    source: String,
    status: String,
    event_id: String,
    reject_reason: String,
    attempts: i64,
}

impl From<DeliveryRow> for RowView {
    fn from(r: DeliveryRow) -> Self {
        RowView {
            id: r.id,
            short: short(r.id),
            received: fmt_time(r.received_at),
            source: r.source,
            status: r.status,
            event_id: r.event_id.unwrap_or_default(),
            reject_reason: r.reject_reason.unwrap_or_default(),
            attempts: r.attempts,
        }
    }
}

#[derive(Deserialize, Default)]
struct ListQuery {
    source: Option<String>,
    status: Option<String>,
    page: Option<i64>,
}

impl ListQuery {
    fn filter(&self) -> ListFilter {
        ListFilter {
            source: self.source.clone().filter(|s| !s.is_empty()),
            status: self
                .status
                .clone()
                .filter(|s| STATUSES.contains(&s.as_str())),
        }
    }
    fn page(&self) -> i64 {
        self.page.unwrap_or(0).clamp(0, 100_000)
    }
}

struct Pager {
    rows: Vec<RowView>,
    page: i64,
    page_label: i64,
    prev_href: Option<String>,
    next_href: Option<String>,
}

async fn load_rows(st: &AppState, q: &ListQuery) -> Result<Pager, Fail> {
    let filter = q.filter();
    let page = q.page();
    let mut rows = inspect::list_deliveries(&st.pool, &filter, PAGE_SIZE + 1, page * PAGE_SIZE)
        .await
        .map_err(db_fail)?;
    let has_next = rows.len() as i64 > PAGE_SIZE;
    rows.truncate(PAGE_SIZE as usize);

    let href = |p: i64| {
        let mut qs = format!("/ui?page={p}");
        if let Some(s) = &filter.source {
            qs.push_str(&format!("&source={}", urlencode(s)));
        }
        if let Some(s) = &filter.status {
            qs.push_str(&format!("&status={}", urlencode(s)));
        }
        qs
    };
    Ok(Pager {
        rows: rows.into_iter().map(RowView::from).collect(),
        page,
        page_label: page + 1,
        prev_href: (page > 0).then(|| href(page - 1)),
        next_href: has_next.then(|| href(page + 1)),
    })
}

/// Source and status values come from config names and a fixed list, so this
/// only has to cover the characters those can contain.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "list.html")]
struct ListPage {
    nav: &'static str,
    sources: Vec<String>,
    statuses: [&'static str; 4],
    source: String,
    status: String,
    rows: Vec<RowView>,
    page: i64,
    page_label: i64,
    prev_href: Option<String>,
    next_href: Option<String>,
}

#[derive(Template)]
#[template(path = "rows.html")]
struct RowsPartial {
    rows: Vec<RowView>,
    page_label: i64,
    prev_href: Option<String>,
    next_href: Option<String>,
}

async fn list_page(State(st): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let pager = match load_rows(&st, &q).await {
        Ok(p) => p,
        Err(r) => return r.into_response(),
    };
    let filter = q.filter();
    render(&ListPage {
        nav: "deliveries",
        sources: st.config.sources.iter().map(|s| s.name.clone()).collect(),
        statuses: STATUSES,
        source: filter.source.unwrap_or_default(),
        status: filter.status.unwrap_or_default(),
        rows: pager.rows,
        page: pager.page,
        page_label: pager.page_label,
        prev_href: pager.prev_href,
        next_href: pager.next_href,
    })
}

async fn rows_partial(State(st): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    match load_rows(&st, &q).await {
        Ok(p) => render(&RowsPartial {
            rows: p.rows,
            page_label: p.page_label,
            prev_href: p.prev_href,
            next_href: p.next_href,
        }),
        Err(r) => r.into_response(),
    }
}

// ---- detail ----------------------------------------------------------------

struct JobView {
    target: String,
    url: String,
    status: String,
    attempts: i32,
    replay: bool,
    last: String,
}

struct TimelineEntry {
    time: String,
    class: &'static str,
    title: String,
    detail: String,
}

struct ActivityView {
    id: Uuid,
    can_replay: bool,
    jobs: Vec<JobView>,
    timeline: Vec<TimelineEntry>,
}

struct DetailView {
    id: Uuid,
    short: String,
    source: String,
    status: String,
    received: String,
    verification: String,
    reject_reason: String,
    event_id: String,
    body_size: String,
    body: String,
    headers: Vec<(String, String)>,
}

/// Pretty JSON when the body is JSON, the text when it is UTF-8, and a short
/// hex preview otherwise.
fn render_body(body: &[u8]) -> String {
    let shown = &body[..body.len().min(BODY_PREVIEW_BYTES)];
    let mut text = if let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) {
        serde_json::to_string_pretty(&json).unwrap_or_default()
    } else if let Ok(s) = std::str::from_utf8(shown) {
        s.to_string()
    } else {
        let preview: Vec<String> = body.iter().take(256).map(|b| format!("{b:02x}")).collect();
        format!("binary body, first bytes in hex:\n{}", preview.join(" "))
    };
    if text.len() > BODY_PREVIEW_BYTES {
        let mut end = BODY_PREVIEW_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n... cut here, the stored copy is complete");
    } else if body.len() > BODY_PREVIEW_BYTES && std::str::from_utf8(shown).is_ok() {
        text.push_str("\n... cut here, the stored copy is complete");
    }
    text
}

fn size_label(n: usize) -> String {
    if n >= 1024 {
        format!("{:.1} KiB ({n} bytes)", n as f64 / 1024.0)
    } else {
        format!("{n} bytes")
    }
}

fn job_status(jobs: &[JobRow], verification: &str) -> &'static str {
    if verification == "rejected" {
        return "rejected";
    }
    // Newest job per target decides.
    let mut latest: Vec<&JobRow> = Vec::new();
    for j in jobs {
        match latest.iter_mut().find(|l| l.target == j.target) {
            Some(slot) => {
                if (j.created_at, j.id) > (slot.created_at, slot.id) {
                    *slot = j;
                }
            }
            None => latest.push(j),
        }
    }
    if latest.iter().any(|j| j.status == "dead") {
        "dead"
    } else if latest
        .iter()
        .any(|j| j.status == "pending" || j.status == "in_flight")
    {
        "pending"
    } else {
        "done"
    }
}

fn build_activity(
    d: &DeliveryDetail,
    jobs: Vec<JobRow>,
    attempts: Vec<AttemptRow>,
) -> ActivityView {
    let mut timeline = vec![TimelineEntry {
        time: fmt_time(d.received_at),
        class: if d.verification == "rejected" {
            "bad"
        } else {
            "ok"
        },
        title: if d.verification == "rejected" {
            "Rejected".to_string()
        } else {
            "Received and verified".to_string()
        },
        detail: d.reject_reason.clone().unwrap_or_default(),
    }];

    let mut dated: Vec<(DateTime<Utc>, TimelineEntry)> = Vec::new();
    for a in &attempts {
        let tag = if a.replay { " (replay)" } else { "" };
        let mut detail = match (a.http_status, &a.error) {
            (_, Some(e)) => format!("{e} in {}", fmt_ms(i64::from(a.duration_ms))),
            (Some(s), None) => format!("HTTP {s} in {}", fmt_ms(i64::from(a.duration_ms))),
            (None, None) => format!("in {}", fmt_ms(i64::from(a.duration_ms))),
        };
        if let Some(ms) = a.retry_in_ms {
            detail.push_str(&format!(", next try in {}", fmt_ms(ms)));
        }
        let (class, verb) = match a.outcome.as_str() {
            "ok" => ("ok", "delivered"),
            "dead" => ("bad", "gave up"),
            _ => ("warn", "failed"),
        };
        dated.push((
            a.started_at,
            TimelineEntry {
                time: fmt_time(a.started_at),
                class,
                title: format!("{} try {} {verb}{tag}", a.target, a.attempt),
                detail,
            },
        ));
    }
    for j in &jobs {
        match j.status.as_str() {
            "pending" => dated.push((
                j.next_attempt_at,
                TimelineEntry {
                    time: fmt_time(j.next_attempt_at),
                    class: "wait",
                    title: format!("{} waiting for try {}", j.target, j.attempts + 1),
                    detail: "scheduled".to_string(),
                },
            )),
            "in_flight" => dated.push((
                Utc::now(),
                TimelineEntry {
                    time: "now".to_string(),
                    class: "wait",
                    title: format!("{} try {} in flight", j.target, j.attempts),
                    detail: String::new(),
                },
            )),
            _ => {}
        }
    }
    dated.sort_by_key(|(t, _)| *t);
    timeline.extend(dated.into_iter().map(|(_, e)| e));

    ActivityView {
        id: d.id,
        can_replay: d.verification == "verified",
        jobs: jobs
            .into_iter()
            .map(|j| JobView {
                target: j.target,
                url: j.url,
                status: j.status,
                attempts: j.attempts,
                replay: j.replay,
                last: match (j.last_status, j.last_error) {
                    (_, Some(e)) => e,
                    (Some(s), None) => format!("HTTP {s}"),
                    (None, None) => "not tried yet".to_string(),
                },
            })
            .collect(),
        timeline,
    }
}

#[derive(Template)]
#[template(path = "detail.html")]
struct DetailPage {
    nav: &'static str,
    d: DetailView,
    a: ActivityView,
}

#[derive(Template)]
#[template(path = "activity.html")]
struct ActivityPartial {
    a: ActivityView,
}

async fn load_activity(
    st: &AppState,
    id: Uuid,
) -> Result<Option<(DeliveryDetail, ActivityView, &'static str)>, Fail> {
    let Some(d) = inspect::get_delivery(&st.pool, id).await.map_err(db_fail)? else {
        return Ok(None);
    };
    let jobs = inspect::jobs_for(&st.pool, id).await.map_err(db_fail)?;
    let attempts = inspect::attempts_for(&st.pool, id).await.map_err(db_fail)?;
    let status = job_status(&jobs, &d.verification);
    let activity = build_activity(&d, jobs, attempts);
    Ok(Some((d, activity, status)))
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "No such delivery.").into_response()
}

async fn detail_page(State(st): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let (d, a, status) = match load_activity(&st, id).await {
        Ok(Some(x)) => x,
        Ok(None) => return not_found(),
        Err(r) => return r.into_response(),
    };
    render(&DetailPage {
        nav: "deliveries",
        d: DetailView {
            id: d.id,
            short: short(d.id),
            source: d.source.clone(),
            status: status.to_string(),
            received: fmt_time(d.received_at),
            verification: d.verification.clone(),
            reject_reason: d.reject_reason.clone().unwrap_or_default(),
            event_id: d.event_id.clone().unwrap_or_default(),
            body_size: size_label(d.body.len()),
            body: render_body(&d.body),
            headers: d.headers,
        },
        a,
    })
}

async fn activity_partial(State(st): State<AppState>, Path(id): Path<Uuid>) -> Response {
    match load_activity(&st, id).await {
        Ok(Some((_, a, _))) => render(&ActivityPartial { a }),
        Ok(None) => not_found(),
        Err(r) => r.into_response(),
    }
}

async fn replay_delivery(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(r) = require_htmx(&headers) {
        return r.into_response();
    }
    let source = match inspect::get_delivery(&st.pool, id).await {
        Ok(Some(d)) => d.source,
        Ok(None) => return not_found(),
        Err(e) => return db_error(e),
    };
    let Some(source) = st.config.source(&source) else {
        return (
            StatusCode::CONFLICT,
            "This source is no longer in the config.",
        )
            .into_response();
    };
    let targets: Vec<(String, String)> = source
        .targets
        .iter()
        .map(|t| (t.name.clone(), t.url.clone()))
        .collect();
    match queue::replay_delivery(&st.pool, id, &targets).await {
        Ok(0) => (
            StatusCode::CONFLICT,
            "Rejected deliveries cannot be replayed.",
        )
            .into_response(),
        Ok(n) => {
            tracing::info!(delivery = %id, jobs = n, "replay requested from the ui");
            st.wake.notify_waiters();
            activity_partial(State(st), Path(id)).await
        }
        Err(e) => db_error(e),
    }
}

// ---- dead letters ----------------------------------------------------------

struct DeadView {
    job_id: Uuid,
    delivery_id: Uuid,
    short: String,
    when: String,
    source: String,
    target: String,
    url: String,
    attempts: i32,
    error: String,
}

impl From<DeadRow> for DeadView {
    fn from(r: DeadRow) -> Self {
        DeadView {
            job_id: r.job_id,
            delivery_id: r.delivery_id,
            short: short(r.delivery_id),
            when: r.finished_at.map(fmt_time).unwrap_or_default(),
            source: r.source,
            target: r.target,
            url: r.url,
            attempts: r.attempts,
            error: match (r.last_status, r.last_error) {
                (_, Some(e)) => e,
                (Some(s), None) => format!("HTTP {s}"),
                (None, None) => String::new(),
            },
        }
    }
}

#[derive(Template)]
#[template(path = "dead.html")]
struct DeadPage {
    nav: &'static str,
    rows: Vec<DeadView>,
}

async fn dead_page(State(st): State<AppState>) -> Response {
    match inspect::dead_jobs(&st.pool, 200).await {
        Ok(rows) => render(&DeadPage {
            nav: "dead",
            rows: rows.into_iter().map(DeadView::from).collect(),
        }),
        Err(e) => db_error(e),
    }
}

#[derive(Template)]
#[template(path = "queued_row.html")]
struct QueuedRow {
    message: String,
}

async fn replay_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<Uuid>,
) -> Response {
    if let Err(r) = require_htmx(&headers) {
        return r.into_response();
    }
    let (delivery_id, target_name) = match inspect::job_origin(&st.pool, job_id).await {
        Ok(Some(x)) => x,
        Ok(None) => return (StatusCode::NOT_FOUND, "No such job.").into_response(),
        Err(e) => return db_error(e),
    };
    let source_name = match inspect::get_delivery(&st.pool, delivery_id).await {
        Ok(Some(d)) => d.source,
        Ok(None) => return not_found(),
        Err(e) => return db_error(e),
    };
    let target = st
        .config
        .source(&source_name)
        .and_then(|s| s.targets.iter().find(|t| t.name == target_name));
    let Some(target) = target else {
        return (
            StatusCode::CONFLICT,
            "This target is no longer in the config.",
        )
            .into_response();
    };
    let one = [(target.name.clone(), target.url.clone())];
    match queue::replay_delivery(&st.pool, delivery_id, &one).await {
        Ok(_) => {
            tracing::info!(delivery = %delivery_id, target = %target_name, "dead job replayed from the ui");
            st.wake.notify_waiters();
            render(&QueuedRow {
                message: format!(
                    "Queued again for {target_name}. It leaves this list once it is retried."
                ),
            })
        }
        Err(e) => db_error(e),
    }
}

// ---- static files ----------------------------------------------------------

async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/app.css"),
    )
}

async fn htmx() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../static/htmx.min.js"),
    )
}
