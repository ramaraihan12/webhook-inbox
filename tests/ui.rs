mod common;

use std::time::Duration;

use common::{TestApp, TestDb, count, fast_config, wait_for_count};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "test-admin-token";
const WAIT: Duration = Duration::from_secs(10);

fn get(app: &TestApp, path: &str) -> reqwest::RequestBuilder {
    app.client
        .get(format!("{}{path}", app.base))
        .bearer_auth(TOKEN)
}

async fn body_of(res: reqwest::Response) -> String {
    res.text().await.unwrap()
}

async fn target_that(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn the_ui_needs_the_admin_token() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &fast_config("http://127.0.0.1:9/x", 3)).await;

    let none = app
        .client
        .get(format!("{}/ui", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), 401);
    assert!(
        none.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .starts_with("Basic")
    );

    let wrong = app
        .client
        .get(format!("{}/ui", app.base))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    assert_eq!(get(&app, "/ui").send().await.unwrap().status(), 200);

    // A browser sends Basic auth, with the token as the password.
    let basic = app
        .client
        .get(format!("{}/ui", app.base))
        .basic_auth("admin", Some(TOKEN))
        .send()
        .await
        .unwrap();
    assert_eq!(basic.status(), 200);
    let wrong_basic = app
        .client
        .get(format!("{}/ui", app.base))
        .basic_auth(TOKEN, Some("x"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        wrong_basic.status(),
        401,
        "the token is the password, not the user"
    );

    // The same holds for every page and for the actions.
    for path in ["/ui/dead", "/ui/rows", "/ui/static/app.css"] {
        let res = app
            .client
            .get(format!("{}{path}", app.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "{path}");
    }
    db.cleanup().await;
}

#[tokio::test]
async fn the_ui_is_off_without_an_admin_token() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app =
        TestApp::start_with_token(&db.pool, &fast_config("http://127.0.0.1:9/x", 3), None).await;
    let res = app
        .client
        .get(format!("{}/ui", app.base))
        .bearer_auth("anything")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
    db.cleanup().await;
}

#[tokio::test]
async fn ui_responses_carry_security_headers_and_static_files_are_served() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &fast_config("http://127.0.0.1:9/x", 3)).await;

    let page = get(&app, "/ui").send().await.unwrap();
    let csp = page.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && !csp.contains("unsafe-inline"));
    assert_eq!(page.headers()["x-content-type-options"], "nosniff");
    assert_eq!(page.headers()["cache-control"], "no-store");
    let html = body_of(page).await;
    assert!(
        html.contains("/ui/static/htmx.min.js"),
        "htmx is served locally, not from a CDN"
    );
    assert!(!html.contains("unpkg") && !html.contains("jsdelivr"));

    let js = get(&app, "/ui/static/htmx.min.js").send().await.unwrap();
    assert_eq!(js.status(), 200);
    assert!(
        js.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript")
    );
    assert!(body_of(js).await.contains("htmx"));
    let css = get(&app, "/ui/static/app.css").send().await.unwrap();
    assert!(
        css.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("text/css")
    );
    db.cleanup().await;
}

#[tokio::test]
async fn the_list_filters_by_source_and_status() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = target_that(200).await;
    let app = TestApp::start(&db.pool, &fast_config(&format!("{}/hook", server.uri()), 3)).await;

    app.post_github(b"{\"n\":1}", "e1").await;
    // A rejected one, from a caller with the wrong secret.
    app.client
        .post(format!("{}/in/github", app.base))
        .body("{}")
        .send()
        .await
        .unwrap();

    let all = body_of(get(&app, "/ui").send().await.unwrap()).await;
    assert!(
        all.contains("badge pending"),
        "queued, no workers running yet"
    );
    assert!(all.contains("badge rejected"));

    let rejected = body_of(get(&app, "/ui/rows?status=rejected").send().await.unwrap()).await;
    assert!(rejected.contains("badge rejected") && !rejected.contains("badge pending"));
    assert!(rejected.contains("signature check") || rejected.contains("missing header"));

    let pending = body_of(get(&app, "/ui/rows?status=pending").send().await.unwrap()).await;
    assert!(pending.contains("badge pending") && !pending.contains("badge rejected"));

    let none = body_of(get(&app, "/ui/rows?source=nope").send().await.unwrap()).await;
    assert!(none.contains("No deliveries match"));

    // Done once a worker has run.
    let workers = app.spawn_workers();
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;
    let done = body_of(
        get(&app, "/ui/rows?status=done&source=github")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(done.contains("badge done") && !done.contains("badge rejected"));
    db.cleanup().await;
}

#[tokio::test]
async fn the_detail_page_shows_pretty_json_headers_and_a_timeline() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = target_that(500).await;
    let app = TestApp::start(&db.pool, &fast_config(&format!("{}/hook", server.uri()), 2)).await;
    let workers = app.spawn_workers();

    let res = app
        .post_github(
            br#"{"order":"INV-9","note":"<script>alert(1)</script>"}"#,
            "e1",
        )
        .await;
    let id = res.headers()["x-inbox-delivery-id"]
        .to_str()
        .unwrap()
        .to_string();
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'dead'",
        1,
        WAIT,
    )
    .await;
    workers.stop().await;

    let html = body_of(
        get(&app, &format!("/ui/deliveries/{id}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(html.contains("badge dead"));
    assert!(
        html.contains("&#34;order&#34;: &#34;INV-9&#34;,\n"),
        "pretty printed, one key per line"
    );
    assert!(
        !html.contains("<script>alert(1)</script>"),
        "the body is escaped"
    );
    assert!(html.contains("&#60;script&#62;"));
    assert!(html.contains("x-github-delivery"));
    assert!(html.contains("app try 1 failed"));
    assert!(html.contains("app try 2 gave up"));
    assert!(html.contains("HTTP 500"));
    assert!(html.contains("Replay to all targets"));

    let missing = get(&app, "/ui/deliveries/00000000-0000-0000-0000-000000000000")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let bad = get(&app, "/ui/deliveries/not-a-uuid").send().await.unwrap();
    assert_eq!(bad.status(), 400);
    db.cleanup().await;
}

#[tokio::test]
async fn replay_needs_the_htmx_header_and_queues_a_new_job() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = target_that(200).await;
    let app = TestApp::start(&db.pool, &fast_config(&format!("{}/hook", server.uri()), 3)).await;
    let workers = app.spawn_workers();

    let res = app.post_github(b"{\"n\":1}", "e1").await;
    let id = res.headers()["x-inbox-delivery-id"]
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

    let url = format!("{}/ui/deliveries/{id}/replay", app.base);
    // What a cross-site form post would look like: no custom header.
    let forged = app
        .client
        .post(&url)
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 400);
    assert_eq!(count(&db.pool, "select count(*) from jobs").await, 1);

    let ok = app
        .client
        .post(&url)
        .bearer_auth(TOKEN)
        .header("hx-request", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert!(body_of(ok).await.contains("Timeline"));

    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'done'",
        2,
        WAIT,
    )
    .await;
    workers.stop().await;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    let html = body_of(
        get(&app, &format!("/ui/deliveries/{id}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(html.contains("(replay)"));
    db.cleanup().await;
}

#[tokio::test]
async fn rejected_deliveries_cannot_be_replayed_from_the_ui() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let app = TestApp::start(&db.pool, &fast_config("http://127.0.0.1:9/x", 3)).await;
    app.client
        .post(format!("{}/in/github", app.base))
        .body("{}")
        .send()
        .await
        .unwrap();
    let id: uuid::Uuid = sqlx::query_scalar("select id from deliveries")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let res = app
        .client
        .post(format!("{}/ui/deliveries/{id}/replay", app.base))
        .bearer_auth(TOKEN)
        .header("hx-request", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 409);
    assert_eq!(count(&db.pool, "select count(*) from jobs").await, 0);

    let html = body_of(
        get(&app, &format!("/ui/deliveries/{id}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(html.contains("badge rejected"));
    assert!(!html.contains("Replay to all targets"));
    db.cleanup().await;
}

#[tokio::test]
async fn dead_letters_list_dead_jobs_and_replay_takes_them_off_the_list() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let server = target_that(503).await;
    let app = TestApp::start(&db.pool, &fast_config(&format!("{}/hook", server.uri()), 2)).await;
    let workers = app.spawn_workers();

    let empty = body_of(get(&app, "/ui/dead").send().await.unwrap()).await;
    assert!(empty.contains("No dead jobs"));

    app.post_github(b"{\"n\":1}", "e1").await;
    wait_for_count(
        &db.pool,
        "select count(*) from jobs where status = 'dead'",
        1,
        WAIT,
    )
    .await;
    // Stopped, so the replayed job below stays waiting instead of failing again.
    workers.stop().await;

    let page = body_of(get(&app, "/ui/dead").send().await.unwrap()).await;
    assert!(page.contains("HTTP 503"));
    assert!(page.contains("hx-post=\"/ui/jobs/"));
    let job_id: uuid::Uuid = sqlx::query_scalar("select id from jobs")
        .fetch_one(&db.pool)
        .await
        .unwrap();

    let url = format!("{}/ui/jobs/{job_id}/replay", app.base);
    let forged = app
        .client
        .post(&url)
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 400);

    let res = app
        .client
        .post(&url)
        .bearer_auth(TOKEN)
        .header("hx-request", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(body_of(res).await.contains("Queued again for app"));

    let after = body_of(get(&app, "/ui/dead").send().await.unwrap()).await;
    assert!(
        after.contains("No dead jobs"),
        "a replayed job leaves the dead-letter list"
    );
    db.cleanup().await;
}
