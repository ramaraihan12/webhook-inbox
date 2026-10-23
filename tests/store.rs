mod common;

use common::TestDb;
use webhook_inbox::store::{self, NewDelivery, Stored};

fn targets() -> Vec<(String, String)> {
    vec![
        ("app".into(), "http://app/in".into()),
        ("audit".into(), "http://audit/in".into()),
    ]
}

fn delivery<'a>(event_id: Option<&'a str>, body: &'a [u8]) -> NewDelivery<'a> {
    NewDelivery {
        source: "github",
        event_id,
        headers: &[],
        body,
    }
}

async fn count(pool: &sqlx::PgPool, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {table}")))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn stores_one_job_per_target() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let stored = store::insert_verified(&db.pool, &delivery(Some("e1"), b"{}"), &targets())
        .await
        .unwrap();
    assert!(matches!(stored, Stored::Created(_)));
    assert_eq!(count(&db.pool, "deliveries").await, 1);
    assert_eq!(count(&db.pool, "jobs").await, 2);
    db.cleanup().await;
}

#[tokio::test]
async fn a_repeated_event_id_is_a_duplicate_and_writes_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    store::insert_verified(&db.pool, &delivery(Some("e1"), b"{}"), &targets())
        .await
        .unwrap();
    let again = store::insert_verified(&db.pool, &delivery(Some("e1"), b"{}"), &targets())
        .await
        .unwrap();
    assert_eq!(again, Stored::Duplicate);
    assert_eq!(count(&db.pool, "deliveries").await, 1);
    assert_eq!(count(&db.pool, "jobs").await, 2);
    db.cleanup().await;
}

#[tokio::test]
async fn the_same_event_id_in_another_source_is_not_a_duplicate() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    store::insert_verified(&db.pool, &delivery(Some("e1"), b"{}"), &targets())
        .await
        .unwrap();
    let other = NewDelivery {
        source: "xendit",
        ..delivery(Some("e1"), b"{}")
    };
    let stored = store::insert_verified(&db.pool, &other, &targets())
        .await
        .unwrap();
    assert!(matches!(stored, Stored::Created(_)));
    db.cleanup().await;
}

#[tokio::test]
async fn deliveries_without_an_event_id_are_never_duplicates() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for _ in 0..3 {
        let stored = store::insert_verified(&db.pool, &delivery(None, b"{}"), &targets())
            .await
            .unwrap();
        assert!(matches!(stored, Stored::Created(_)));
    }
    assert_eq!(count(&db.pool, "deliveries").await, 3);
    db.cleanup().await;
}

#[tokio::test]
async fn rejected_deliveries_are_capped_to_the_newest() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for i in 0..7 {
        let body = format!("bad-{i}");
        store::insert_rejected(
            &db.pool,
            &delivery(None, body.as_bytes()),
            "signature does not match",
            3,
        )
        .await
        .unwrap();
    }
    assert_eq!(count(&db.pool, "deliveries").await, 3);
    let bodies: Vec<Vec<u8>> =
        sqlx::query_scalar("select body from deliveries order by received_at")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        bodies,
        [b"bad-4".to_vec(), b"bad-5".to_vec(), b"bad-6".to_vec()]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn the_rejected_cap_never_removes_verified_deliveries() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    store::insert_verified(&db.pool, &delivery(Some("e1"), b"{}"), &targets())
        .await
        .unwrap();
    for _ in 0..5 {
        store::insert_rejected(&db.pool, &delivery(None, b"x"), "bad", 1)
            .await
            .unwrap();
    }
    let verified: i64 =
        sqlx::query_scalar("select count(*) from deliveries where verification = 'verified'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(verified, 1);
    assert_eq!(count(&db.pool, "deliveries").await, 2);
    db.cleanup().await;
}

#[tokio::test]
async fn the_rejected_cap_holds_under_concurrent_inserts() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut tasks = Vec::new();
    for i in 0..40 {
        let pool = db.pool.clone();
        tasks.push(tokio::spawn(async move {
            let body = format!("bad-{i}");
            let d = NewDelivery {
                source: "github",
                event_id: None,
                headers: &[],
                body: body.as_bytes(),
            };
            store::insert_rejected(&pool, &d, "bad", 10).await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    // Racing trims may leave a few extra rows, the next insert settles it.
    store::insert_rejected(&db.pool, &delivery(None, b"last"), "bad", 10)
        .await
        .unwrap();
    assert_eq!(count(&db.pool, "deliveries").await, 10);
    db.cleanup().await;
}
