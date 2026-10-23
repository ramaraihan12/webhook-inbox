use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use webhook_inbox::app::{self, AppState, SHUTDOWN_GRACE};
use webhook_inbox::config::Config;
use webhook_inbox::worker::Worker;
use webhook_inbox::{store, telemetry};

#[derive(Parser)]
#[command(
    name = "webhook-inbox",
    version,
    about = "Receive webhooks, verify them, store them and forward them with retries"
)]
struct Cli {
    /// Path to the config file.
    #[arg(
        long,
        short,
        env = "INBOX_CONFIG",
        default_value = "inbox.toml",
        global = true
    )]
    config: PathBuf,

    /// Postgres connection string.
    #[arg(long, env = "DATABASE_URL", global = true, hide_env_values = true)]
    database_url: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the receiver, the workers and the UI (the default).
    Serve,
    /// Delete old deliveries together with their jobs and attempts.
    Prune {
        /// Age threshold, for example 30d or 12h.
        #[arg(long)]
        older_than: String,
        /// Count what would be deleted and delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Load the config, check it and print what it contains.
    CheckConfig,
    /// Ask a running instance for /healthz and exit 1 if it does not answer
    /// 200. Used as the container health check, since the image has no curl.
    Healthcheck {
        #[arg(long, default_value = "http://127.0.0.1:8020/healthz")]
        url: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    telemetry::init_logging();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(&cli.config, cli.database_url).await,
        Command::Prune {
            older_than,
            dry_run,
        } => prune(cli.database_url, &older_than, dry_run).await,
        Command::CheckConfig => check_config(&cli.config),
        Command::Healthcheck { url } => healthcheck(&url).await,
    }
}

fn database_url(url: Option<String>) -> Result<String> {
    url.context("DATABASE_URL is not set")
}

async fn healthcheck(url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?;
    let res = client.get(url).send().await.context("no answer")?;
    anyhow::ensure!(
        res.status().is_success(),
        "health check returned {}",
        res.status()
    );
    Ok(())
}

fn check_config(path: &Path) -> Result<()> {
    let config = Config::load(path).with_context(|| format!("loading {}", path.display()))?;
    println!("listen {}", config.server.listen);
    for source in &config.sources {
        println!("/in/{}", source.name);
        for target in &source.targets {
            println!("  -> {} {}", target.name, target.url);
        }
    }
    Ok(())
}

async fn prune(url: Option<String>, older_than: &str, dry_run: bool) -> Result<()> {
    let age = humantime::parse_duration(older_than)
        .with_context(|| format!("{older_than:?} is not a duration like 30d or 12h"))?;
    let pool = store::connect(&database_url(url)?, 2).await?;
    let n = store::prune(&pool, age, dry_run).await?;
    if dry_run {
        println!("would delete {n} deliveries older than {older_than}");
    } else {
        println!("deleted {n} deliveries older than {older_than}");
    }
    Ok(())
}

async fn serve(config_path: &Path, url: Option<String>) -> Result<()> {
    let config = Arc::new(
        Config::load(config_path).with_context(|| format!("loading {}", config_path.display()))?,
    );
    let url = database_url(url)?;
    let pool_size = config.worker.count as u32 + 16;
    let pool = store::connect(&url, pool_size)
        .await
        .context("connecting to postgres")?;
    store::migrate(&pool).await.context("running migrations")?;

    let wake = Arc::new(Notify::new());
    let state = AppState {
        pool: pool.clone(),
        config: config.clone(),
        wake: wake.clone(),
        metrics: telemetry::install_metrics(),
        admin_token: std::env::var("ADMIN_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(|t| Arc::new(t.into())),
    };

    let shutdown = CancellationToken::new();
    let worker = Worker::new(pool.clone(), config.clone(), wake);
    let workers = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    let listener = TcpListener::bind(config.server.listen)
        .await
        .with_context(|| format!("binding {}", config.server.listen))?;
    tracing::info!(
        listen = %config.server.listen,
        sources = config.sources.len(),
        workers = config.worker.count,
        "webhook-inbox is up"
    );

    axum::serve(listener, app::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("no longer accepting requests, letting in-flight deliveries finish");
    shutdown.cancel();
    match tokio::time::timeout(SHUTDOWN_GRACE, workers).await {
        Ok(_) => tracing::info!("workers finished, bye"),
        Err(_) => tracing::warn!(
            grace = ?SHUTDOWN_GRACE,
            "workers still busy after the grace period, unfinished jobs are picked up again after their lease"
        ),
    }
    pool.close().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
