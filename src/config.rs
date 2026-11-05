//! `inbox.toml`: sources, verifiers, targets and the knobs for the worker.
//!
//! Secrets never live in the file. A source names the environment variable that
//! holds its secret (`secret_env`) and target headers can use `${VAR}`. Loading
//! fails with a message that names the source or target at fault.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use http::{HeaderName, HeaderValue};
use serde::Deserialize;

use crate::event_id::EventIdSource;
use crate::verify::{
    Encoding, GithubVerifier, HmacVerifier, MidtransVerifier, Verifier, XenditVerifier,
};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

/// Room a delivery needs between its timeout and the end of its lease, to
/// record the result before another worker may take the job.
const LEASE_MARGIN: Duration = Duration::from_secs(1);

fn invalid<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(msg.into()))
}

#[derive(Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub worker: WorkerConfig,
    pub retry: RetryConfig,
    /// How many rejected deliveries to keep for debugging.
    pub rejected_keep: i64,
    pub sources: Vec<Source>,
}

#[derive(Debug)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub max_body_bytes: usize,
    pub request_timeout: Duration,
    /// Request headers that are not stored, so they are never shown in the UI
    /// or forwarded. Lowercase, and `x-vercel-*` matches by prefix. For what a
    /// platform adds about the caller, which is more than it documents.
    pub hide_headers: Vec<String>,
    /// Serve the public page at `/` that says how to sign in. For a demo only.
    pub landing_page: bool,
    pub rate_limit: Option<RateLimitConfig>,
}

impl ServerConfig {
    pub fn hides_header(&self, name: &str) -> bool {
        self.hide_headers.iter().any(|h| match h.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == h,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub per_minute: u32,
    /// Holds the caller's address. It has to be a header the proxy in front
    /// sets itself, because one a caller can send is one a caller can vary.
    pub client_ip_header: HeaderName,
}

#[derive(Debug)]
pub struct WorkerConfig {
    /// Concurrent claim loops in this process.
    pub count: usize,
    /// Jobs claimed per round trip, and delivered concurrently by one loop.
    pub batch: i64,
    pub poll_interval: Duration,
    /// Instead of polling, rest until the next job is due or a new one arrives,
    /// so an empty queue costs no queries at all. `poll_interval` is then only
    /// the wait after a failed query.
    pub sleep_when_idle: bool,
    /// How long a claimed job stays locked before another worker may take it,
    /// which is what recovers jobs from a crashed process.
    pub lease: Duration,
}

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: i32,
    pub base: Duration,
    pub cap: Duration,
    /// Upper bound for a `Retry-After` the target asks for.
    pub max_retry_after: Duration,
}

#[derive(Debug)]
pub struct Source {
    pub name: String,
    pub verifier: Verifier,
    pub event_id: Option<EventIdSource>,
    pub targets: Vec<Target>,
}

#[derive(Debug)]
pub struct Target {
    pub name: String,
    pub url: String,
    /// The url as written in `inbox.toml`, with `${VAR}` unexpanded. Safe to
    /// print, unlike `url`, which may carry a token.
    pub url_template: String,
    pub timeout: Duration,
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Config::from_toml(&text, &|name| std::env::var(name).ok())
    }

    /// `env` looks up a variable, so tests do not touch the process environment.
    pub fn from_toml(
        text: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(text)?;
        raw.resolve(env)
    }

    pub fn source(&self, name: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.name == name)
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    worker: RawWorker,
    #[serde(default)]
    retry: RawRetry,
    #[serde(default)]
    rejected: RawRejected,
    #[serde(default, rename = "source")]
    sources: Vec<RawSource>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawServer {
    listen: Option<String>,
    max_body_bytes: Option<usize>,
    request_timeout: Option<String>,
    #[serde(default)]
    hide_headers: Vec<String>,
    #[serde(default)]
    landing_page: bool,
    rate_limit: Option<RawRateLimit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRateLimit {
    per_minute: u32,
    client_ip_header: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawWorker {
    count: Option<usize>,
    batch: Option<i64>,
    poll_interval: Option<String>,
    #[serde(default)]
    sleep_when_idle: bool,
    lease: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRetry {
    max_attempts: Option<i32>,
    base: Option<String>,
    cap: Option<String>,
    max_retry_after: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRejected {
    keep: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    name: String,
    verifier: String,
    secret_env: Option<String>,
    header: Option<String>,
    encoding: Option<String>,
    prefix: Option<String>,
    tolerance: Option<String>,
    event_id: Option<RawEventId>,
    #[serde(default, rename = "target")]
    targets: Vec<RawTarget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventId {
    header: Option<String>,
    json_pointer: Option<String>,
    json_pointers: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    name: String,
    url: String,
    timeout: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

fn duration(
    field: &str,
    value: &Option<String>,
    default: Duration,
) -> Result<Duration, ConfigError> {
    match value {
        None => Ok(default),
        Some(s) => match humantime::parse_duration(s) {
            Ok(d) if !d.is_zero() => Ok(d),
            Ok(_) => invalid(format!("{field}: must be longer than zero")),
            Err(e) => invalid(format!("{field}: {s:?} is not a duration ({e})")),
        },
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl RawConfig {
    fn resolve(self, env: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        let s = self.server;
        let listen = s.listen.unwrap_or_else(|| "0.0.0.0:8020".to_string());
        let listen: SocketAddr = match listen.parse() {
            Ok(a) => a,
            Err(_) => return invalid(format!("server.listen: {listen:?} is not host:port")),
        };
        let max_body_bytes = s.max_body_bytes.unwrap_or(1024 * 1024);
        if max_body_bytes == 0 {
            return invalid("server.max_body_bytes: must be above zero");
        }
        let rate_limit = match s.rate_limit {
            None => None,
            Some(r) => {
                if r.per_minute == 0 {
                    return invalid("server.rate_limit.per_minute: must be above zero");
                }
                let Ok(client_ip_header) = HeaderName::from_bytes(r.client_ip_header.as_bytes())
                else {
                    return invalid(format!(
                        "server.rate_limit.client_ip_header: {:?} is not a valid header name",
                        r.client_ip_header
                    ));
                };
                Some(RateLimitConfig {
                    per_minute: r.per_minute,
                    client_ip_header,
                })
            }
        };
        let server = ServerConfig {
            listen,
            max_body_bytes,
            request_timeout: duration(
                "server.request_timeout",
                &s.request_timeout,
                Duration::from_secs(10),
            )?,
            hide_headers: s
                .hide_headers
                .iter()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
            landing_page: s.landing_page,
            rate_limit,
        };

        let w = self.worker;
        let worker = WorkerConfig {
            count: w.count.unwrap_or(4),
            batch: w.batch.unwrap_or(8),
            poll_interval: duration(
                "worker.poll_interval",
                &w.poll_interval,
                Duration::from_secs(1),
            )?,
            sleep_when_idle: w.sleep_when_idle,
            lease: duration("worker.lease", &w.lease, Duration::from_secs(120))?,
        };
        if worker.count == 0 {
            return invalid("worker.count: must be at least 1");
        }
        if !(1..=500).contains(&worker.batch) {
            return invalid("worker.batch: must be between 1 and 500");
        }

        let r = self.retry;
        let retry = RetryConfig {
            max_attempts: r.max_attempts.unwrap_or(8),
            base: duration("retry.base", &r.base, Duration::from_secs(2))?,
            cap: duration("retry.cap", &r.cap, Duration::from_secs(3600))?,
            max_retry_after: duration(
                "retry.max_retry_after",
                &r.max_retry_after,
                Duration::from_secs(3600),
            )?,
        };
        if !(1..=50).contains(&retry.max_attempts) {
            return invalid("retry.max_attempts: must be between 1 and 50");
        }
        if retry.cap < retry.base {
            return invalid("retry.cap: must not be shorter than retry.base");
        }

        let rejected_keep = self.rejected.keep.unwrap_or(500);
        if rejected_keep < 0 {
            return invalid("rejected.keep: must not be negative");
        }

        if self.sources.is_empty() {
            return invalid("no [[source]] entries, there is nothing to receive");
        }
        let mut sources: Vec<Source> = Vec::new();
        for raw in self.sources {
            if sources.iter().any(|s| s.name == raw.name) {
                return invalid(format!("source {:?} is defined twice", raw.name));
            }
            let name = raw.name.clone();
            let source = resolve_source(raw, env)
                .map_err(|e| ConfigError::Invalid(format!("source {name:?}: {e}")))?;
            // A request that can outlive the lease would be claimed a second
            // time while it is still running, and the target would get two.
            for t in &source.targets {
                if t.timeout + LEASE_MARGIN >= worker.lease {
                    return invalid(format!(
                        "source {name:?}: target {:?}: timeout {} must be at least {} shorter than worker.lease ({})",
                        t.name,
                        humantime::format_duration(t.timeout),
                        humantime::format_duration(LEASE_MARGIN),
                        humantime::format_duration(worker.lease),
                    ));
                }
            }
            sources.push(source);
        }

        Ok(Config {
            server,
            worker,
            retry,
            rejected_keep,
            sources,
        })
    }
}

fn resolve_source(
    raw: RawSource,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Source, ConfigError> {
    if !valid_name(&raw.name) {
        return invalid(
            "name: use lowercase letters, digits, - and _ (it becomes part of /in/<name>)",
        );
    }

    let secret = || -> Result<String, ConfigError> {
        let var = match &raw.secret_env {
            Some(v) => v,
            None => return invalid("secret_env: required for this verifier"),
        };
        match env(var) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => invalid(format!(
                "secret_env: environment variable {var} is not set or empty"
            )),
        }
    };

    let verifier = match raw.verifier.as_str() {
        "github" => Verifier::Github(GithubVerifier::new(secret()?)),
        "midtrans" => Verifier::Midtrans(MidtransVerifier::new(secret()?)),
        "xendit" => Verifier::Xendit(XenditVerifier::new(secret()?)),
        "hmac" => {
            let header = match &raw.header {
                Some(h) => h,
                None => return invalid("header: required for the hmac verifier"),
            };
            if HeaderName::from_bytes(header.as_bytes()).is_err() {
                return invalid(format!("header: {header:?} is not a valid header name"));
            }
            let mut v = HmacVerifier::new(secret()?, header.as_str());
            v = v.encoding(match raw.encoding.as_deref() {
                None | Some("hex") => Encoding::Hex,
                Some("base64") => Encoding::Base64,
                Some(other) => return invalid(format!("encoding: {other:?} is not hex or base64")),
            });
            if let Some(prefix) = &raw.prefix {
                v = v.prefix(prefix.as_str());
            }
            if raw.tolerance.is_some() {
                v = v.tolerance(duration("tolerance", &raw.tolerance, Duration::ZERO)?);
            }
            Verifier::Hmac(v)
        }
        other => {
            return invalid(format!(
                "verifier: {other:?} is not one of github, hmac, midtrans, xendit"
            ));
        }
    };
    if raw.verifier != "hmac"
        && (raw.header.is_some()
            || raw.encoding.is_some()
            || raw.prefix.is_some()
            || raw.tolerance.is_some())
    {
        return invalid("header, encoding, prefix and tolerance only apply to the hmac verifier");
    }

    let event_id = match raw.event_id {
        None => None,
        Some(e) => Some(match (e.header, e.json_pointer, e.json_pointers) {
            (Some(h), None, None) => {
                if HeaderName::from_bytes(h.as_bytes()).is_err() {
                    return invalid(format!("event_id.header: {h:?} is not a valid header name"));
                }
                EventIdSource::Header(h.to_ascii_lowercase())
            }
            (None, Some(p), None) => EventIdSource::JsonPointers(vec![p]),
            (None, None, Some(ps)) if !ps.is_empty() => EventIdSource::JsonPointers(ps),
            _ => {
                return invalid(
                    "event_id: set exactly one of header, json_pointer or json_pointers",
                );
            }
        }),
    };
    if let Some(EventIdSource::JsonPointers(ps)) = &event_id
        && let Some(bad) = ps.iter().find(|p| !p.starts_with('/'))
    {
        return invalid(format!("event_id: json pointer {bad:?} must start with /"));
    }

    if raw.targets.is_empty() {
        return invalid("needs at least one [[source.target]]");
    }
    let mut targets: Vec<Target> = Vec::new();
    for t in raw.targets {
        if targets.iter().any(|x| x.name == t.name) {
            return invalid(format!("target {:?} is defined twice", t.name));
        }
        let name = t.name.clone();
        let target = resolve_target(t, env)
            .map_err(|e| ConfigError::Invalid(format!("target {name:?}: {e}")))?;
        targets.push(target);
    }

    Ok(Source {
        name: raw.name,
        verifier,
        event_id,
        targets,
    })
}

fn resolve_target(
    raw: RawTarget,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Target, ConfigError> {
    if !valid_name(&raw.name) {
        return invalid("name: use lowercase letters, digits, - and _");
    }
    let url = expand_env(&raw.url, env).map_err(|e| ConfigError::Invalid(format!("url: {e}")))?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        // Show the template, not the expansion, which may hold a token.
        return invalid(format!(
            "url: {:?} must start with http:// or https://",
            raw.url
        ));
    }
    let timeout = duration("timeout", &raw.timeout, Duration::from_secs(10))?;

    let mut headers = Vec::new();
    for (key, value) in raw.headers {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| {
            ConfigError::Invalid(format!("headers: {key:?} is not a valid header name"))
        })?;
        let expanded = expand_env(&value, env)
            .map_err(|e| ConfigError::Invalid(format!("headers.{key}: {e}")))?;
        let mut value = HeaderValue::from_str(&expanded).map_err(|_| {
            ConfigError::Invalid(format!("headers.{key}: not a valid header value"))
        })?;
        // Target headers often carry tokens, so keep them out of debug output.
        value.set_sensitive(true);
        headers.push((name, value));
    }

    Ok(Target {
        name: raw.name,
        url,
        url_template: raw.url,
        timeout,
        headers,
    })
}

/// Replaces `${VAR}` with the variable's value. A missing variable is an
/// error, so a typo does not silently send an empty token.
fn expand_env(value: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or("unclosed ${")?;
        let var = &after[..end];
        let val = env(var).ok_or_else(|| format!("environment variable {var} is not set"))?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "GITHUB_SECRET" => Some("gh-secret".into()),
            "MIDTRANS_KEY" => Some("SB-Mid-server-demo".into()),
            "APP_TOKEN" => Some("s3cret".into()),
            _ => None,
        }
    }

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_toml(text, &env)
    }

    fn err(text: &str) -> String {
        parse(text).unwrap_err().to_string()
    }

    const MINIMAL: &str = r#"
        [[source]]
        name = "github"
        verifier = "github"
        secret_env = "GITHUB_SECRET"
        [[source.target]]
        name = "app"
        url = "http://localhost:3000/hooks/github"
    "#;

    #[test]
    fn minimal_config_gets_defaults() {
        let c = parse(MINIMAL).unwrap();
        assert_eq!(c.server.listen.port(), 8020);
        assert_eq!(c.server.max_body_bytes, 1024 * 1024);
        assert_eq!(c.worker.count, 4);
        assert_eq!(c.retry.max_attempts, 8);
        assert_eq!(c.retry.base, Duration::from_secs(2));
        assert_eq!(c.retry.cap, Duration::from_secs(3600));
        assert_eq!(c.rejected_keep, 500);
        let s = c.source("github").unwrap();
        assert_eq!(s.targets[0].timeout, Duration::from_secs(10));
        assert!(s.event_id.is_none());
        assert!(c.source("nope").is_none());
    }

    #[test]
    fn the_public_settings_are_off_by_default() {
        let c = parse(MINIMAL).unwrap();
        assert!(c.server.hide_headers.is_empty());
        assert!(!c.server.landing_page);
        assert!(c.server.rate_limit.is_none());
    }

    #[test]
    fn the_public_settings_parse() {
        let text = format!(
            r#"
            [server]
            landing_page = true
            hide_headers = ["X-Forwarded-For"]
            [server.rate_limit]
            per_minute = 20
            client_ip_header = "CF-Connecting-IP"
            {MINIMAL}
            "#
        );
        let c = parse(&text).unwrap();
        assert_eq!(c.server.hide_headers, ["x-forwarded-for"]);
        assert!(c.server.landing_page);
        let limit = c.server.rate_limit.unwrap();
        assert_eq!(limit.per_minute, 20);
        assert_eq!(limit.client_ip_header, "cf-connecting-ip");
    }

    #[test]
    fn hidden_headers_match_by_name_or_by_prefix() {
        let text = format!(
            r#"
            [server]
            hide_headers = ["Forwarded", "X-Vercel-*"]
            {MINIMAL}
            "#
        );
        let server = parse(&text).unwrap().server;
        assert!(server.hides_header("forwarded"));
        assert!(server.hides_header("x-vercel-oidc-token"));
        assert!(!server.hides_header("forwarded-for"));
        assert!(!server.hides_header("x-hub-signature-256"));
    }

    #[test]
    fn a_rate_limit_of_zero_or_a_bad_header_is_refused() {
        let with = |per_minute: u32, header: &str| {
            format!(
                r#"
                [server.rate_limit]
                per_minute = {per_minute}
                client_ip_header = "{header}"
                {MINIMAL}
                "#
            )
        };
        assert!(err(&with(0, "x-real-ip")).contains("per_minute"));
        assert!(err(&with(5, "not a header")).contains("client_ip_header"));
    }

    #[test]
    fn the_public_demo_only_forwards_to_its_own_sink() {
        let text = include_str!("../deploy/vercel/inbox.toml");
        let c = Config::from_toml(text, &|_| Some("demo-value".to_string())).unwrap();
        assert!(c.server.rate_limit.is_some());
        assert!(c.worker.sleep_when_idle, "a poll would keep Neon awake");
        assert!(c.server.hides_header("x-vercel-oidc-token"));
        assert!(c.server.hides_header("x-forwarded-for"));
        assert!(!c.sources.is_empty());
        for target in c.sources.iter().flat_map(|s| &s.targets) {
            assert!(
                target.url.starts_with("http://127.0.0.1:8021/"),
                "{} points at {}",
                target.name,
                target.url
            );
        }
    }

    #[test]
    fn full_config_parses() {
        let c = parse(
            r#"
            [server]
            listen = "127.0.0.1:9000"
            max_body_bytes = 2048
            request_timeout = "5s"
            [worker]
            count = 2
            batch = 4
            poll_interval = "250ms"
            lease = "30s"
            [retry]
            max_attempts = 3
            base = "500ms"
            cap = "1m"
            max_retry_after = "10m"
            [rejected]
            keep = 10

            [[source]]
            name = "midtrans"
            verifier = "midtrans"
            secret_env = "MIDTRANS_KEY"
            event_id = { json_pointers = ["/transaction_id", "/transaction_status"] }
            [[source.target]]
            name = "billing"
            url = "https://billing.internal/hooks/midtrans"
            timeout = "3s"
            headers = { authorization = "Bearer ${APP_TOKEN}" }
            [[source.target]]
            name = "audit"
            url = "http://audit:8080/in"

            [[source]]
            name = "partner"
            verifier = "hmac"
            secret_env = "GITHUB_SECRET"
            header = "X-Partner-Signature"
            encoding = "base64"
            prefix = "v1="
            tolerance = "5m"
            event_id = { header = "X-Event-Id" }
            [[source.target]]
            name = "app"
            url = "http://app/in"
            "#,
        )
        .unwrap();
        assert_eq!(c.server.listen.to_string(), "127.0.0.1:9000");
        assert_eq!(c.worker.poll_interval, Duration::from_millis(250));
        assert_eq!(c.retry.cap, Duration::from_secs(60));
        let m = c.source("midtrans").unwrap();
        assert_eq!(m.targets.len(), 2);
        assert_eq!(m.targets[0].headers[0].1.to_str().unwrap(), "Bearer s3cret");
        assert_eq!(
            m.event_id,
            Some(EventIdSource::JsonPointers(vec![
                "/transaction_id".into(),
                "/transaction_status".into()
            ]))
        );
        let p = c.source("partner").unwrap();
        assert_eq!(p.event_id, Some(EventIdSource::Header("x-event-id".into())));
    }

    #[test]
    fn debug_output_hides_secrets_and_target_headers() {
        let c = parse(
            r#"
            [[source]]
            name = "github"
            verifier = "github"
            secret_env = "GITHUB_SECRET"
            [[source.target]]
            name = "app"
            url = "http://app/in"
            headers = { authorization = "Bearer ${APP_TOKEN}" }
            "#,
        )
        .unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("gh-secret"));
        assert!(!shown.contains("s3cret"));
    }

    #[test]
    fn rejects_bad_files_with_a_useful_message() {
        assert!(err("").contains("no [[source]]"));
        assert!(err("[nope]\nx = 1").contains("unknown field"));
        assert!(err(&MINIMAL.replace("GITHUB_SECRET", "UNSET_VAR")).contains("UNSET_VAR"));
        assert!(
            err(&MINIMAL.replace(
                "\"github\"\n        secret_env",
                "\"stripe\"\n        secret_env"
            ))
            .contains("not one of")
        );
        assert!(
            err(&MINIMAL.replace("name = \"github\"", "name = \"Git Hub\"")).contains("lowercase")
        );
        assert!(
            err(&MINIMAL.replace("http://localhost:3000/hooks/github", "ftp://x"))
                .contains("http://")
        );
    }

    #[test]
    fn rejects_missing_pieces() {
        let no_target = r#"
            [[source]]
            name = "a"
            verifier = "github"
            secret_env = "GITHUB_SECRET"
        "#;
        assert!(err(no_target).contains("at least one"));

        let no_secret = MINIMAL.replace("secret_env = \"GITHUB_SECRET\"", "");
        assert!(err(&no_secret).contains("secret_env: required"));

        let hmac_no_header =
            MINIMAL.replace("\"github\"\n        secret", "\"hmac\"\n        secret");
        assert!(err(&hmac_no_header).contains("header: required"));

        let stray = MINIMAL.replace("secret_env =", "prefix = \"x\"\n        secret_env =");
        assert!(err(&stray).contains("only apply to the hmac"));
    }

    #[test]
    fn rejects_duplicates() {
        let twice = format!("{MINIMAL}{MINIMAL}");
        assert!(err(&twice).contains("defined twice"));
        let two_targets =
            format!("{MINIMAL}\n[[source.target]]\nname = \"app\"\nurl = \"http://x\"\n");
        assert!(err(&two_targets).contains("target \"app\" is defined twice"));
    }

    #[test]
    fn rejects_bad_numbers_and_durations() {
        let with = |extra: &str| format!("{extra}\n{MINIMAL}");
        assert!(err(&with("[retry]\nmax_attempts = 0")).contains("max_attempts"));
        assert!(err(&with("[retry]\nbase = \"soon\"")).contains("not a duration"));
        assert!(err(&with("[retry]\nbase = \"10s\"\ncap = \"1s\"")).contains("retry.cap"));
        assert!(err(&with("[worker]\ncount = 0")).contains("worker.count"));
        assert!(err(&with("[server]\nlisten = \"nowhere\"")).contains("host:port"));
    }

    #[test]
    fn event_id_needs_exactly_one_form() {
        let two = MINIMAL.replace(
            "[[source.target]]",
            "event_id = { header = \"a\", json_pointer = \"/b\" }\n        [[source.target]]",
        );
        assert!(err(&two).contains("exactly one"));
        let bad_pointer = MINIMAL.replace(
            "[[source.target]]",
            "event_id = { json_pointer = \"id\" }\n        [[source.target]]",
        );
        assert!(err(&bad_pointer).contains("must start with /"));
    }

    #[test]
    fn header_templates_fail_on_unset_variables() {
        let t = MINIMAL.to_string() + "\nheaders = { x-token = \"${NOT_SET}\" }\n";
        assert!(err(&t).contains("NOT_SET"));
    }

    #[test]
    fn target_urls_can_use_env_variables() {
        let t = MINIMAL.replace("http://localhost:3000/hooks/github", "${APP_TOKEN}/hooks");
        // The expanded value is not a url, so this also proves the check runs
        // after expansion, and the message shows the template, never the secret.
        let e = err(&t);
        assert!(e.contains("${APP_TOKEN}/hooks"), "{e}");
        assert!(!e.contains("s3cret"), "{e}");
        let ok = MINIMAL.replace(
            "http://localhost:3000/hooks/github",
            "http://${APP_TOKEN}.local/in",
        );
        let c = parse(&ok).unwrap();
        assert_eq!(c.sources[0].targets[0].url, "http://s3cret.local/in");
        assert_eq!(
            c.sources[0].targets[0].url_template,
            "http://${APP_TOKEN}.local/in"
        );
        let unset = MINIMAL.replace("http://localhost:3000/hooks/github", "${NOT_SET}/in");
        assert!(err(&unset).contains("NOT_SET"));
    }

    #[test]
    fn a_target_timeout_must_leave_room_inside_the_lease() {
        let with = |lease: &str, timeout: &str| {
            format!(
                r#"
                [worker]
                lease = "{lease}"
                [[source]]
                name = "github"
                verifier = "github"
                secret_env = "GITHUB_SECRET"
                [[source.target]]
                name = "app"
                url = "http://localhost:3000/hooks/github"
                timeout = "{timeout}"
                "#
            )
        };
        assert!(parse(&with("30s", "10s")).is_ok());
        let e = err(&with("30s", "30s"));
        assert!(e.contains("\"app\"") && e.contains("worker.lease"), "{e}");
        assert!(parse(&with("30s", "29500ms")).is_err());
    }

    #[test]
    fn env_expansion() {
        assert_eq!(expand_env("plain", &env).unwrap(), "plain");
        assert_eq!(
            expand_env("a${APP_TOKEN}b${APP_TOKEN}", &env).unwrap(),
            "as3cretbs3cret"
        );
        assert!(expand_env("${APP_TOKEN", &env).is_err());
    }
}
