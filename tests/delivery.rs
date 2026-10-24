mod common;

use std::collections::HashSet;
use std::time::Duration;

use common::{TestApp, TestDb, count, fast_config, wait_for_count};
use sqlx::PgPool;
use uuid::Uuid;
use webhook_inbox::queue;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BODY: &[u8] = br#"{"order":"INV-2026-001","total":"150000.00"}"#;
const WAIT: Duration = Duration::from_secs(10);

async fn start(server: &MockServer, db: &TestDb, max_attempts: u32) -> TestApp {
    let config = fast_config(&format!("{}/hook", server.uri()), max_attempts);
    TestApp::start(&db.pool, &config).await
}

async fn attempts(pool: &PgPool) -> Vec<(i32, Option<i32>, String)> {
    sqlx::query_as("select attempt, http_status, outcome from attempts order by attempt")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn forwards_the_original_body_with_inbox_headers() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let app = start(&server, &db, 5).await;
    let workers = app.spawn_workers();

    let res = app.post_github(BODY, "gh-1").await;
    let delivery_id = res.headers()["x-inbox-delivery-id"]
        .to_str()
        .unwrap()
        .to_string();
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req.body, BODY, "the body is forwarded byte for byte");
    assert_eq!(
        req.headers["x-inbox-delivery-id"].to_str().unwrap(),
        delivery_id
    );
    assert_eq!(req.headers["x-inbox-attempt"], "1");
    assert_eq!(req.headers["x-inbox-source"], "github");
    assert_eq!(
        req.headers["x-app-key"], "k-123",
        "target headers are added"
    );
    assert_eq!(
        req.headers["x-github-delivery"], "gh-1",
        "provider headers are kept"
    );
    assert_eq!(req.headers["content-type"], "application/json");

    assert_eq!(attempts(&db.pool).await, [(1, Some(200), "ok".to_string())]);
    db.cleanup().await;
}

#[tokio::test]
async fn retries_with_backoff_then_succeeds() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream broke"))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(2)
        .mount(&server)
        .await;
    let app = start(&server, &db, 5).await;
    let workers = app.spawn_workers();

    app.post_github(BODY, "gh-1").await;
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;

    let seen: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.headers["x-inbox-attempt"].to_str().unwrap().to_string())
        .collect();
    assert_eq!(seen, ["1", "2", "3"]);
    assert_eq!(
        attempts(&db.pool).await,
        [
            (1, Some(500), "retry".to_string()),
            (2, Some(500), "retry".to_string()),
            (3, Some(200), "ok".to_string()),
        ]
    );
    let first_error: String = sqlx::query_scalar("select error from attempts where attempt = 1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(first_error, "HTTP 500: upstream broke");
    db.cleanup().await;
}

#[tokio::test]
async fn goes_dead_after_max_attempts() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let app = start(&server, &db, 3).await;
    let workers = app.spawn_workers();

    app.post_github(BODY, "gh-1").await;
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'dead'",
        1,
        WAIT,
    )
    .await;
    // Give a wrongly retrying worker the chance to make a fourth call.
    tokio::time::sleep(Duration::from_millis(400)).await;
    workers.stop().await;

    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert_eq!(
        attempts(&db.pool)
            .await
            .iter()
            .map(|a| a.2.as_str())
            .collect::<Vec<_>>(),
        ["retry", "retry", "dead"]
    );
    let (attempts_made, last_error): (i32, Option<String>) =
        sqlx::query_as("select attempts, last_error from jobs")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(attempts_made, 3);
    assert_eq!(last_error.as_deref(), Some("HTTP 503"));
    db.cleanup().await;
}

#[tokio::test]
async fn honours_retry_after() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(2)
        .mount(&server)
        .await;
    let app = start(&server, &db, 5).await;
    let workers = app.spawn_workers();

    app.post_github(BODY, "gh-1").await;
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;

    // The configured backoff is at most 100ms, so a 1s gap can only come from Retry-After.
    let gap_ms: f64 = sqlx::query_scalar(
        "select extract(epoch from (
           (select started_at from attempts where attempt = 2) -
           (select started_at from attempts where attempt = 1)
         ))::float8 * 1000",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(gap_ms >= 950.0, "second try came after {gap_ms} ms");
    db.cleanup().await;
}

#[tokio::test]
async fn a_slow_target_times_out_and_is_retried() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;
    let app = start(&server, &db, 2).await;
    let workers = app.spawn_workers();

    app.post_github(BODY, "gh-1").await;
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'dead'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;

    let errors: Vec<Option<String>> =
        sqlx::query_scalar("select error from attempts order by attempt")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        errors,
        [Some("timed out".to_string()), Some("timed out".to_string())]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn an_unreachable_target_is_retried_and_the_url_is_not_leaked() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // Nothing listens on port 9.
    let config = fast_config("http://127.0.0.1:9/secret-path?token=abc123", 2);
    let app = TestApp::start(&db.pool, &config).await;
    let workers = app.spawn_workers();

    app.post_github(BODY, "gh-1").await;
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'dead'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;

    let error: String = sqlx::query_scalar("select error from attempts where attempt = 1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(error.starts_with("could not connect"), "{error}");
    assert!(!error.contains("abc123"));
    db.cleanup().await;
}

#[tokio::test]
async fn a_duplicate_event_is_forwarded_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let app = start(&server, &db, 5).await;
    let workers = app.spawn_workers();

    for _ in 0..3 {
        app.post_github(BODY, "gh-same").await;
    }
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        1,
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    workers.stop().await;

    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    db.cleanup().await;
}

#[tokio::test]
async fn replay_forwards_the_stored_body_again_from_attempt_one() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let app = start(&server, &db, 5).await;
    let workers = app.spawn_workers();

    let res = app.post_github(BODY, "gh-1").await;
    let id: Uuid = res.headers()["x-inbox-delivery-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        1,
        WAIT,
    )
    .await;

    let targets = vec![("app".to_string(), format!("{}/hook", server.uri()))];
    assert_eq!(
        queue::replay_delivery(&db.pool, id, &targets)
            .await
            .unwrap(),
        1
    );
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        2,
        WAIT,
    )
    .await;
    workers.stop().await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].body, BODY);
    assert_eq!(requests[1].headers["x-inbox-attempt"], "1");
    assert_eq!(
        count(&db.pool, "select count(*) from jobs where replay").await,
        1
    );
    assert_eq!(count(&db.pool, "select count(*) from deliveries").await, 1);
    db.cleanup().await;
}

#[tokio::test]
async fn rejected_deliveries_cannot_be_replayed() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    let app = start(&server, &db, 5).await;
    app.client
        .post(format!("{}/in/github", app.base))
        .body(BODY.to_vec())
        .send()
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar("select id from deliveries")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let targets = vec![("app".to_string(), format!("{}/hook", server.uri()))];
    assert_eq!(
        queue::replay_delivery(&db.pool, id, &targets)
            .await
            .unwrap(),
        0
    );
    assert_eq!(count(&db.pool, "select count(*) from jobs").await, 0);
    db.cleanup().await;
}

#[tokio::test]
async fn two_worker_pools_never_deliver_the_same_attempt_twice() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(5)))
        .mount(&server)
        .await;
    let app = start(&server, &db, 5).await;

    // Queue first, so both pools start with the same pile of due jobs.
    const N: i64 = 120;
    for i in 0..N {
        app.post_github(BODY, &format!("gh-{i}")).await;
    }
    let a = app.spawn_workers();
    let b = app.spawn_workers();
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        N,
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.stop().await;
    b.stop().await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len() as i64,
        N,
        "every job was delivered exactly once"
    );
    let keys: HashSet<(String, String)> = requests
        .iter()
        .map(|r| {
            (
                r.headers["x-inbox-delivery-id"]
                    .to_str()
                    .unwrap()
                    .to_string(),
                r.headers["x-inbox-attempt"].to_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        keys.len() as i64,
        N,
        "no (delivery, attempt) pair was sent twice"
    );
    assert_eq!(count(&db.pool, "select count(*) from attempts").await, N);
    db.cleanup().await;
}

#[tokio::test]
async fn claim_skips_rows_another_transaction_has_locked() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    let app = start(&server, &db, 5).await;
    for i in 0..4 {
        app.post_github(BODY, &format!("gh-{i}")).await;
    }

    // Hold two jobs locked in an open transaction, as a busy worker would.
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select id from jobs order by created_at, id limit 2 for update")
        .fetch_all(&mut *tx)
        .await
        .unwrap();

    let claimed = queue::claim_jobs(&db.pool, 10, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(
        claimed.len(),
        2,
        "the two locked jobs are skipped, not waited for"
    );
    tx.rollback().await.unwrap();
    db.cleanup().await;
}

#[tokio::test]
async fn a_job_whose_lease_expired_is_picked_up_again() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let app = start(&server, &db, 5).await;
    app.post_github(BODY, "gh-1").await;

    // A worker claimed the job and then died: in flight, lease in the past.
    sqlx::query(
        "update jobs set status = 'in_flight', attempts = 1, locked_until = now() - interval '1 minute',
             next_attempt_at = now() - interval '1 minute'",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let workers = app.spawn_workers();
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers["x-inbox-attempt"], "2",
        "the lost try still counts"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_worker_that_lost_its_lease_does_not_record_its_attempt() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    let app = start(&server, &db, 5).await;
    app.post_github(BODY, "gh-1").await;

    let mut jobs = queue::claim_jobs(&db.pool, 1, Duration::from_secs(60))
        .await
        .unwrap();
    let job = jobs.pop().unwrap();
    // Someone else reclaimed it in the meantime.
    sqlx::query("update jobs set attempts = attempts + 1")
        .execute(&db.pool)
        .await
        .unwrap();

    let result = queue::AttemptResult {
        verdict: queue::Verdict::Done,
        http_status: Some(200),
        error: None,
        started_at: chrono::Utc::now(),
        duration_ms: 3,
    };
    assert!(!queue::finish_job(&db.pool, &job, &result).await.unwrap());
    assert_eq!(count(&db.pool, "select count(*) from attempts").await, 0);
    assert_eq!(
        elapsed: Duration::from_millis(3),
        count(
            &db.pool,
            "select count(*) from jobs where status = 'in_flight'"
        )
        .await,
        1
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_target_removed_from_the_config_goes_dead() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    let app = start(&server, &db, 5).await;
    app.post_github(BODY, "gh-1").await;
    sqlx::query("update jobs set target = 'gone'")
        .execute(&db.pool)
        .await
        .unwrap();

    let workers = app.spawn_workers();
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'dead'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;

    let error: String = sqlx::query_scalar("select last_error from jobs")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(error.contains("no longer in the config"));
    assert!(server.received_requests().await.unwrap().is_empty());
    db.cleanup().await;
}

#[tokio::test]
async fn shutdown_waits_for_a_delivery_that_is_already_in_flight() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    let app = start(&server, &db, 5).await;
    let workers = app.spawn_workers();

    app.post_github(BODY, "gh-1").await;
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'in_flight'",
        1,
        WAIT,
    )
    .await;
    // The request is out and the target is still thinking. Ask for shutdown now.
    workers.stop().await;

    assert_eq!(
        count(&db.pool, "select count(*) from jobs where status = 'done'").await,
        1
    );
    assert_eq!(
        count(
            &db.pool,
            "select count(*) from attempts where outcome = 'ok'"
        )
        .await,
        1
    );
    db.cleanup().await;
}
