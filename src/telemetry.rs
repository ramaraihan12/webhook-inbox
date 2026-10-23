//! Prometheus metrics and log setup.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Installs the global recorder on first use and returns its handle. Calling it
/// again returns the same handle, which keeps tests that build several apps in
/// one process from fighting over the recorder.
pub fn install_metrics() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .set_buckets_for_metric(
                    Matcher::Suffix("_seconds".to_string()),
                    &[
                        0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
                        5.0, 10.0, 30.0,
                    ],
                )
                .expect("bucket list is not empty")
                .install_recorder()
                .expect("no other recorder is installed")
        })
        .clone()
}

pub fn init_logging() {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,tower_http=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}
