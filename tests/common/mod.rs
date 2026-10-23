//! Shared setup for the database tests.
//!
//! Each test gets its own schema in the database named by `TEST_DATABASE_URL`,
//! so tests run in parallel without seeing each other's rows. Without the
//! variable, `TestDb::new` returns `None` and the test returns early.

#![allow(dead_code)]

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};

pub struct TestDb {
    pub pool: PgPool,
    admin: PgPool,
    schema: String,
}

impl TestDb {
    pub async fn new() -> Option<TestDb> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("TEST_DATABASE_URL is not set, skipping");
            return None;
        };
        let schema = format!("t_{}", uuid::Uuid::new_v4().simple());

        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        admin
            .execute(sqlx::AssertSqlSafe(format!("create schema {schema}")))
            .await
            .unwrap();

        let search_path = format!("set search_path to {schema}");
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .after_connect(move |conn, _| {
                let stmt = search_path.clone();
                Box::pin(async move {
                    conn.execute(sqlx::AssertSqlSafe(stmt)).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        webhook_inbox::store::migrate(&pool).await.unwrap();

        Some(TestDb {
            pool,
            admin,
            schema,
        })
    }

    pub async fn cleanup(self) {
        self.pool.close().await;
        let _ = self
            .admin
            .execute(sqlx::AssertSqlSafe(format!(
                "drop schema {} cascade",
                self.schema
            )))
            .await;
    }
}
