//! The settings a public demo turns on: the rate limit, hidden headers and the
//! landing page. All of them are off unless the config asks for them.

mod common;

use common::{TestApp, TestDb, count, github_signature};

const TOKEN: &str = "test-admin-token";

const SOURCES: &str = r#"
    [[source]]
    name = "github"
    verifier = "github"
    secret_env = "GITHUB_SECRET"
    [[source.target]]
    name = "app"
    url = "http://127.0.0.1:9/never"
"#;

fn public_config() -> String {
    format!(
        r#"
        [server]
        landing_page = true
        hide_headers = ["X-Forwarded-For", "cf-connecting-ip"]
        [server.rate_limit]
        per_minute = 2
        client_ip_header = "cf-connecting-ip"
        {SOURCES}
        "#
    )
}

const BODY: &[u8] = br#"{"action":"opened"}"#;

async fn post_from(app: &TestApp, ip: &str) -> reqwest::Response {
    app.client
        .post(format!("{}/in/github", app.base))
        .header("cf-connecting-ip", ip)
        .header("x-forwarded-for", format!("{ip}, 10.0.0.1"))
        .header("x-hub-signature-256", github_signature("gh-secret", BODY))
        .body(BODY.to_vec())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_client_over_the_limit_gets_429_and_nothing_is_stored() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &public_config()).await;

    assert_eq!(post_from(&app, "203.0.113.7").await.status(), 200);
    assert_eq!(post_from(&app, "203.0.113.7").await.status(), 200);
    let refused = post_from(&app, "203.0.113.7").await;
    assert_eq!(refused.status(), 429);
    assert_eq!(refused.headers()["retry-after"], "60");

    assert_eq!(count(&db.pool, "select count(*) from deliveries").await, 2);
    db.cleanup().await;
}

#[tokio::test]
async fn each_client_address_has_its_own_count() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &public_config()).await;

    for _ in 0..3 {
        post_from(&app, "203.0.113.7").await;
    }
    assert_eq!(post_from(&app, "203.0.113.8").await.status(), 200);
    db.cleanup().await;
}

#[tokio::test]
async fn requests_without_the_address_header_share_one_count() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &public_config()).await;

    let send = || {
        app.client
            .post(format!("{}/in/github", app.base))
            .header("x-hub-signature-256", github_signature("gh-secret", BODY))
            .body(BODY.to_vec())
            .send()
    };
    assert_eq!(send().await.unwrap().status(), 200);
    assert_eq!(send().await.unwrap().status(), 200);
    assert_eq!(send().await.unwrap().status(), 429);
    db.cleanup().await;
}

#[tokio::test]
async fn replay_counts_against_the_same_limit() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &public_config()).await;
    let ip = "203.0.113.9";

    let res = post_from(&app, ip).await;
    let id = res.headers()["x-inbox-delivery-id"]
        .to_str()
        .unwrap()
        .to_string();
    let replay = || {
        app.client
            .post(format!("{}/ui/deliveries/{id}/replay", app.base))
            .bearer_auth(TOKEN)
            .header("hx-request", "true")
            .header("cf-connecting-ip", ip)
            .send()
    };

    assert_eq!(replay().await.unwrap().status(), 200);
    assert_eq!(replay().await.unwrap().status(), 429);
    assert_eq!(count(&db.pool, "select count(*) from jobs").await, 2);
    db.cleanup().await;
}

#[tokio::test]
async fn without_a_rate_limit_nothing_is_refused() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, SOURCES).await;

    for _ in 0..5 {
        assert_eq!(post_from(&app, "203.0.113.7").await.status(), 200);
    }
    db.cleanup().await;
}

#[tokio::test]
async fn hidden_headers_are_not_stored_for_accepted_or_rejected_deliveries() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &public_config()).await;

    assert_eq!(post_from(&app, "203.0.113.7").await.status(), 200);
    let bad = app
        .client
        .post(format!("{}/in/github", app.base))
        .header("cf-connecting-ip", "203.0.113.8")
        .header("x-forwarded-for", "203.0.113.8")
        .header("x-hub-signature-256", "sha256=00")
        .body(BODY.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);

    let rows: Vec<(String,)> = sqlx::query_as("select headers::text from deliveries")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for (headers,) in rows {
        assert!(headers.contains("x-hub-signature-256"), "{headers}");
        assert!(!headers.contains("203.0.113"), "{headers}");
    }
    db.cleanup().await;
}

#[tokio::test]
async fn the_landing_page_needs_no_token_and_says_how_to_sign_in() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &public_config()).await;

    let res = app
        .client
        .get(format!("{}/", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        res.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(res.headers().contains_key("content-security-policy"));
    let page = res.text().await.unwrap();
    assert!(page.contains("demo-admin-token"));
    assert!(page.contains(r#"href="/ui""#));
    db.cleanup().await;
}

#[tokio::test]
async fn there_is_no_landing_page_unless_the_config_asks_for_one() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, SOURCES).await;

    // Unknown paths go through the UI guard, so the token is needed to see the 404.
    let res = app
        .client
        .get(format!("{}/", app.base))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    db.cleanup().await;
}
