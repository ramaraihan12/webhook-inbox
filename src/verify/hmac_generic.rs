use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use http::HeaderMap;
use sha2::Sha256;

use super::{Secret, VerifyError, ct_eq, header_str};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Hex,
    Base64,
}

/// Generic HMAC-SHA256 over the raw body.
///
/// Two header shapes are supported:
///
/// * plain: the header holds the signature, optionally behind a prefix such as
///   `sha256=`.
/// * timestamped (set `tolerance`): a Stripe-style `t=<unix>,v1=<hex>[,v1=<hex>]`
///   header, signed over `"<t>.<body>"`. The timestamp must be within
///   `tolerance` of now, which stops an old capture from being replayed.
#[derive(Debug, Clone)]
pub struct HmacVerifier {
    secret: Secret,
    header: String,
    encoding: Encoding,
    prefix: Option<String>,
    tolerance: Option<Duration>,
}

impl HmacVerifier {
    pub fn new(secret: impl Into<Secret>, header: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            header: header.into().to_ascii_lowercase(),
            encoding: Encoding::Hex,
            prefix: None,
            tolerance: None,
        }
    }

    pub fn encoding(mut self, encoding: Encoding) -> Self {
        self.encoding = encoding;
        self
    }

    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    pub fn tolerance(mut self, tolerance: Duration) -> Self {
        self.tolerance = Some(tolerance);
        self
    }

    /// `now` is unix seconds, passed in so tests do not depend on the clock.
    pub fn verify(&self, headers: &HeaderMap, body: &[u8], now: i64) -> Result<(), VerifyError> {
        let value = header_str(headers, &self.header)?;
        match self.tolerance {
            Some(tolerance) => self.verify_timestamped(value, body, now, tolerance),
            None => self.verify_plain(value, body),
        }
    }

    fn verify_plain(&self, value: &str, body: &[u8]) -> Result<(), VerifyError> {
        let sig = match &self.prefix {
            Some(prefix) => value
                .strip_prefix(prefix.as_str())
                .ok_or(VerifyError::Malformed)?,
            None => value,
        };
        let given = self.decode(sig.trim())?;
        if ct_eq(&self.mac(&[body]), &given) {
            Ok(())
        } else {
            Err(VerifyError::Mismatch)
        }
    }

    fn verify_timestamped(
        &self,
        value: &str,
        body: &[u8],
        now: i64,
        tolerance: Duration,
    ) -> Result<(), VerifyError> {
        let mut timestamp: Option<&str> = None;
        let mut candidates: Vec<&str> = Vec::new();
        for part in value.split(',') {
            match part.trim().split_once('=') {
                Some(("t", v)) => timestamp = Some(v),
                Some(("v1", v)) => candidates.push(v),
                _ => {}
            }
        }
        let timestamp = timestamp.ok_or(VerifyError::Malformed)?;
        let t: i64 = timestamp.parse().map_err(|_| VerifyError::Malformed)?;
        if candidates.is_empty() {
            return Err(VerifyError::Malformed);
        }

        // Check the signature first, then the window. Every candidate is
        // always compared, so the loop does not stop at the first match.
        let expected = self.mac(&[timestamp.as_bytes(), b".", body]);
        let mut matched = false;
        for candidate in candidates {
            if let Ok(given) = self.decode(candidate) {
                matched |= ct_eq(&expected, &given);
            }
        }
        if !matched {
            return Err(VerifyError::Mismatch);
        }
        if now.abs_diff(t) > tolerance.as_secs() {
            return Err(VerifyError::StaleTimestamp);
        }
        Ok(())
    }

    fn mac(&self, parts: &[&[u8]]) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_bytes())
            .expect("hmac takes any key length");
        for part in parts {
            mac.update(part);
        }
        mac.finalize().into_bytes().to_vec()
    }

    fn decode(&self, s: &str) -> Result<Vec<u8>, VerifyError> {
        let bytes = match self.encoding {
            Encoding::Hex => hex::decode(s).map_err(|_| VerifyError::Malformed)?,
            Encoding::Base64 => STANDARD.decode(s).map_err(|_| VerifyError::Malformed)?,
        };
        if bytes.is_empty() {
            return Err(VerifyError::Malformed);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_demo";
    const BODY: &[u8] = br#"{"event":"paid","invoice":"INV-7"}"#;
    const T: i64 = 1_760_000_000;
    // Computed independently with Python's hmac module.
    const PLAIN_HEX: &str = "f0e1c91829122bc69acb355b9d1b8026e1886442d62d862ff4608934891db9db";
    const PLAIN_B64: &str = "8OHJGCkSK8aayzVbnRuAJuGIZELWLYYv9GCJNIkduds=";
    const STAMPED_HEX: &str = "fff0cee4474f645a8bd0ac460e137fe8455bceb941651ea13bae5f4a841f3994";

    fn headers(name: &'static str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, value.parse().unwrap());
        h
    }

    fn plain() -> HmacVerifier {
        HmacVerifier::new(SECRET, "X-Signature")
    }

    fn stamped() -> HmacVerifier {
        HmacVerifier::new(SECRET, "x-signature").tolerance(Duration::from_secs(300))
    }

    fn stamped_header(t: i64, sig: &str) -> HeaderMap {
        headers("x-signature", &format!("t={t},v1={sig}"))
    }

    #[test]
    fn plain_hex_known_vector() {
        let h = headers("x-signature", PLAIN_HEX);
        assert_eq!(plain().verify(&h, BODY, T), Ok(()));
    }

    #[test]
    fn plain_with_prefix() {
        let v = plain().prefix("sha256=");
        let ok = headers("x-signature", &format!("sha256={PLAIN_HEX}"));
        assert_eq!(v.verify(&ok, BODY, T), Ok(()));
        let bare = headers("x-signature", PLAIN_HEX);
        assert_eq!(v.verify(&bare, BODY, T), Err(VerifyError::Malformed));
    }

    #[test]
    fn plain_base64() {
        let v = plain().encoding(Encoding::Base64);
        assert_eq!(
            v.verify(&headers("x-signature", PLAIN_B64), BODY, T),
            Ok(())
        );
    }

    #[test]
    fn plain_rejects_tampered_body_and_wrong_secret() {
        let h = headers("x-signature", PLAIN_HEX);
        let tampered = br#"{"event":"paid","invoice":"INV-8"}"#;
        assert_eq!(plain().verify(&h, tampered, T), Err(VerifyError::Mismatch));
        let other = HmacVerifier::new("nope", "x-signature");
        assert_eq!(other.verify(&h, BODY, T), Err(VerifyError::Mismatch));
    }

    #[test]
    fn plain_rejects_missing_header_and_garbage() {
        assert_eq!(
            plain().verify(&HeaderMap::new(), BODY, T),
            Err(VerifyError::MissingHeader("x-signature".into()))
        );
        assert_eq!(
            plain().verify(&headers("x-signature", "not hex"), BODY, T),
            Err(VerifyError::Malformed)
        );
    }

    #[test]
    fn timestamped_known_vector() {
        assert_eq!(
            stamped().verify(&stamped_header(T, STAMPED_HEX), BODY, T),
            Ok(())
        );
        // Inside the window on either side.
        assert_eq!(
            stamped().verify(&stamped_header(T, STAMPED_HEX), BODY, T + 300),
            Ok(())
        );
        assert_eq!(
            stamped().verify(&stamped_header(T, STAMPED_HEX), BODY, T - 300),
            Ok(())
        );
    }

    #[test]
    fn timestamped_rejects_a_stale_timestamp() {
        let h = stamped_header(T, STAMPED_HEX);
        assert_eq!(
            stamped().verify(&h, BODY, T + 301),
            Err(VerifyError::StaleTimestamp)
        );
        assert_eq!(
            stamped().verify(&h, BODY, T - 301),
            Err(VerifyError::StaleTimestamp)
        );
    }

    #[test]
    fn timestamped_signature_covers_the_timestamp() {
        // Same signature with a fresher timestamp must not verify.
        let h = stamped_header(T + 10, STAMPED_HEX);
        assert_eq!(
            stamped().verify(&h, BODY, T + 10),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn timestamped_rejects_tampered_body_and_plain_signature() {
        let tampered = br#"{"event":"paid","invoice":"INV-8"}"#;
        let h = stamped_header(T, STAMPED_HEX);
        assert_eq!(
            stamped().verify(&h, tampered, T),
            Err(VerifyError::Mismatch)
        );
        // A signature over the body alone is not a valid timestamped one.
        assert_eq!(
            stamped().verify(&stamped_header(T, PLAIN_HEX), BODY, T),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn timestamped_accepts_any_matching_v1_and_ignores_other_schemes() {
        let value = format!("t={T},v0=abcd,v1=00ff,v1={STAMPED_HEX}");
        let h = headers("x-signature", &value);
        assert_eq!(stamped().verify(&h, BODY, T), Ok(()));
    }

    #[test]
    fn timestamped_rejects_malformed_headers() {
        for value in ["", "v1=abcd", &format!("t={T}"), "t=abc,v1=abcd"] {
            let h = headers("x-signature", value);
            assert_eq!(
                stamped().verify(&h, BODY, T),
                Err(VerifyError::Malformed),
                "{value}"
            );
        }
    }
}
