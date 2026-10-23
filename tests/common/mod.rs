//! Shared setup for the database tests.
//!
//! Each test gets its own schema in the database named by `TEST_DATABASE_URL`,
//! so tests run in parallel without seeing each other's rows. Without the
//! variable, `TestDb::new` returns `None` and the test returns early.

#![allow(dead_code)]

use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use webhook_inbox::app::{self, AppState};
use webhook_inbox::config::Config;

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

/// Secrets the test configs refer to through `secret_env`.
pub fn test_env(name: &str) -> Option<String> {
    match name {
        "GITHUB_SECRET" => Some("gh-secret".to_string()),
        "XENDIT_TOKEN" => Some("xnd-token".to_string()),
        _ => None,
    }
}

pub struct TestApp {
    pub base: String,
    pub state: AppState,
    pub client: reqwest::Client,
}

impl TestApp {
    /// Starts the HTTP side only.
    pub async fn start(pool: &PgPool, toml: &str) -> TestApp {
        let config = Arc::new(Config::from_toml(toml, &test_env).unwrap());
        let state = AppState {
            pool: pool.clone(),
            config,
            wake: Arc::new(Notify::new()),
            metrics: webhook_inbox::telemetry::install_metrics(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app::router(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        TestApp {
            base,
            state,
            client: reqwest::Client::new(),
        }
    }
}

pub fn github_signature(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

pub async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}
