mod common;

use common::{TestApp, TestDb, count, github_signature};

const CONFIG: &str = r#"
    [server]
    max_body_bytes = 2048

    [[source]]
    name = "github"
    verifier = "github"
    secret_env = "GITHUB_SECRET"
    event_id = { header = "x-github-delivery" }
    [[source.target]]
    name = "app"
    url = "http://127.0.0.1:9/never"
    [[source.target]]
    name = "audit"
    url = "http://127.0.0.1:9/never"

    [[source]]
    name = "xendit"
    verifier = "xendit"
    secret_env = "XENDIT_TOKEN"
    [[source.target]]
    name = "app"
    url = "http://127.0.0.1:9/never"
"#;

const BODY: &[u8] = br#"{"action":"opened","number":7}"#;

async fn post_github(
    app: &TestApp,
    body: &[u8],
    delivery: &str,
    secret: &str,
) -> reqwest::Response {
    app.client
        .post(format!("{}/in/github", app.base))
        .header("x-hub-signature-256", github_signature(secret, body))
        .header("x-github-delivery", delivery)
        .header("content-type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_signed_delivery_is_stored_with_a_job_per_target() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, CONFIG).await;

    let res = post_github(&app, BODY, "d-1", "gh-secret").await;
    assert_eq!(res.status(), 200);
    assert!(res.headers().contains_key("x-inbox-delivery-id"));
    assert!(!res.headers().contains_key("x-inbox-duplicate"));

    assert_eq!(
        count(
            &db.pool,
            "select count(*) from deliveries where verification = 'verified'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &db.pool,
            "select count(*) from jobs where status = 'pending'"
        )
        .await,
        2
    );
    let stored: Vec<u8> = sqlx::query_scalar("select body from deliveries")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(stored, BODY, "the raw body is kept byte for byte");
    db.cleanup().await;
}

#[tokio::test]
async fn a_bad_signature_is_401_and_kept_as_rejected_without_jobs() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, CONFIG).await;

    let res = post_github(&app, BODY, "d-1", "wrong-secret").await;
    assert_eq!(res.status(), 401);

    let (verification, reason): (String, Option<String>) =
        sqlx::query_as("select verification, reject_reason from deliveries")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(verification, "rejected");
    assert_eq!(reason.as_deref(), Some("signature does not match"));
    assert_eq!(count(&db.pool, "select count(*) from jobs").await, 0);
    db.cleanup().await;
}

#[tokio::test]
async fn a_missing_signature_header_is_401() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, CONFIG).await;
    let res = app
        .client
        .post(format!("{}/in/github", app.base))
        .body(BODY.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    db.cleanup().await;
}

#[tokio::test]
async fn a_repeated_event_id_is_200_marked_duplicate_and_not_queued_again() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, CONFIG).await;

    assert_eq!(
        post_github(&app, BODY, "d-1", "gh-secret").await.status(),
        200
    );
    let again = post_github(&app, BODY, "d-1", "gh-secret").await;
    assert_eq!(again.status(), 200);
    assert_eq!(again.headers()["x-inbox-duplicate"], "true");

    assert_eq!(count(&db.pool, "select count(*) from deliveries").await, 1);
    assert_eq!(count(&db.pool, "select count(*) from jobs").await, 2);

    // A different event id is a new delivery.
    assert_eq!(
        post_github(&app, BODY, "d-2", "gh-secret").await.status(),
        200
    );
    assert_eq!(count(&db.pool, "select count(*) from deliveries").await, 2);
    db.cleanup().await;
}

#[tokio::test]
async fn unknown_sources_are_404_and_other_methods_are_405() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, CONFIG).await;
    let res = app
        .client
        .post(format!("{}/in/nope", app.base))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let res = app
        .client
        .get(format!("{}/in/github", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 405);
    assert_eq!(count(&db.pool, "select count(*) from deliveries").await, 0);
    db.cleanup().await;
}

#[tokio::test]
async fn bodies_over_the_limit_are_413_and_not_stored() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, CONFIG).await;
    let big = vec![b'a'; 4096];
    let res = post_github(&app, &big, "d-1", "gh-secret").await;
    assert_eq!(res.status(), 413);
    assert_eq!(count(&db.pool, "select count(*) from deliveries").await, 0);
    db.cleanup().await;
}

#[tokio::test]
async fn credential_headers_are_not_stored() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, CONFIG).await;
    let res = app
        .client
        .post(format!("{}/in/xendit", app.base))
        .header("x-callback-token", "xnd-token")
        .header("authorization", "Bearer something")
        .header("x-note", "kept")
        .body(r#"{"id":"inv-1"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let headers: serde_json::Value = sqlx::query_scalar("select headers from deliveries")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let text = headers.to_string();
    assert!(text.contains("x-note"));
    assert!(!text.contains("xnd-token"));
    assert!(!text.contains("Bearer something"));
    db.cleanup().await;
}

#[tokio::test]
async fn healthz_and_metrics_respond() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, CONFIG).await;
    post_github(&app, BODY, "d-1", "gh-secret").await;
    post_github(&app, BODY, "d-2", "nope").await;

    let health = app
        .client
        .get(format!("{}/healthz", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);

    let text = app
        .client
        .get(format!("{}/metrics", app.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("inbox_received_total"), "{text}");
    assert!(text.contains(r#"result="accepted""#));
    assert!(text.contains(r#"result="rejected""#));
    assert!(text.contains("inbox_receive_seconds_bucket"));
    assert!(text.contains(r#"inbox_jobs{status="pending"}"#));
    db.cleanup().await;
}
