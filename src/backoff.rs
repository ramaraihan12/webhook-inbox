//! Retry timing: exponential backoff with full jitter, plus `Retry-After`.

use std::time::{Duration, SystemTime};

use crate::config::RetryConfig;

/// Upper bound of the wait after `attempt` failed tries (1 after the first
/// failure): `base * 2^(attempt - 1)`, never above `cap`.
pub fn ceiling(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let shift = attempt.saturating_sub(1).min(32);
    let factor = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
    base.saturating_mul(factor).min(cap)
}

/// Full jitter: a uniform pick in `[0, ceiling]`. `unit` is a random number in
/// `[0, 1]`, passed in so the math can be tested without a random source.
pub fn full_jitter(ceiling: Duration, unit: f64) -> Duration {
    ceiling.mul_f64(unit.clamp(0.0, 1.0))
}

/// How long to wait before the next try. A `Retry-After` from the target is a
/// floor (capped at `max_retry_after`), so jitter never sends us back early.
pub fn next_delay(
    attempt: u32,
    retry: &RetryConfig,
    retry_after: Option<Duration>,
    unit: f64,
) -> Duration {
    let jittered = full_jitter(ceiling(attempt, retry.base, retry.cap), unit);
    match retry_after {
        Some(asked) => jittered.max(asked.min(retry.max_retry_after)),
        None => jittered,
    }
}

/// Parses a `Retry-After` header: delta seconds or an HTTP date. A date in the
/// past means "now", so it gives zero.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retry() -> RetryConfig {
        RetryConfig {
            max_attempts: 8,
            base: Duration::from_secs(2),
            cap: Duration::from_secs(60),
            max_retry_after: Duration::from_secs(300),
        }
    }

    #[test]
    fn ceiling_doubles_then_stops_at_the_cap() {
        let (base, cap) = (Duration::from_secs(2), Duration::from_secs(60));
        let secs: Vec<u64> = (1..=8).map(|a| ceiling(a, base, cap).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn ceiling_does_not_overflow() {
        let (base, cap) = (Duration::from_secs(2), Duration::from_secs(3600));
        assert_eq!(ceiling(0, base, cap), base);
        assert_eq!(ceiling(40, base, cap), cap);
        assert_eq!(ceiling(u32::MAX, base, cap), cap);
        assert_eq!(
            ceiling(10, Duration::MAX, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn jitter_spans_zero_to_the_ceiling() {
        let c = Duration::from_secs(10);
        assert_eq!(full_jitter(c, 0.0), Duration::ZERO);
        assert_eq!(full_jitter(c, 0.5), Duration::from_secs(5));
        assert_eq!(full_jitter(c, 1.0), c);
        assert_eq!(full_jitter(c, 7.0), c);
        assert_eq!(full_jitter(c, -1.0), Duration::ZERO);
    }

    #[test]
    fn retry_after_is_a_floor_not_a_replacement() {
        let r = retry();
        // Ceiling after 2 failures is 4s. Asking for 30s wins over a 1s jitter.
        let d = next_delay(2, &r, Some(Duration::from_secs(30)), 0.25);
        assert_eq!(d, Duration::from_secs(30));
        // A short Retry-After does not shrink a longer jittered wait.
        let d = next_delay(2, &r, Some(Duration::from_secs(1)), 1.0);
        assert_eq!(d, Duration::from_secs(4));
        // Without one, plain jitter.
        assert_eq!(next_delay(2, &r, None, 0.5), Duration::from_secs(2));
    }

    #[test]
    fn retry_after_is_capped() {
        let r = retry();
        let d = next_delay(1, &r, Some(Duration::from_secs(86_400)), 0.0);
        assert_eq!(d, Duration::from_secs(300));
    }

    #[test]
    fn parses_seconds_and_http_dates() {
        let now = httpdate::parse_http_date("Fri, 23 Oct 2026 10:00:00 GMT").unwrap();
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("Fri, 23 Oct 2026 10:01:30 GMT", now),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_retry_after("Fri, 23 Oct 2026 09:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
        assert_eq!(parse_retry_after("", now), None);
    }
}
