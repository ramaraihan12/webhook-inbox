//! Signature checks for incoming webhooks.
//!
//! Every comparison of a secret-derived value goes through [`ct_eq`], which is
//! constant time for slices of the same length and rejects different lengths.

mod github;
mod hmac_generic;
mod midtrans;
mod xendit;

use http::HeaderMap;
use subtle::ConstantTimeEq;

pub use github::GithubVerifier;
pub use hmac_generic::{Encoding, HmacVerifier};
pub use midtrans::MidtransVerifier;
pub use xendit::XenditVerifier;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("missing header {0}")]
    MissingHeader(String),
    #[error("malformed signature")]
    Malformed,
    #[error("signature does not match")]
    Mismatch,
    #[error("timestamp outside the allowed window")]
    StaleTimestamp,
}

/// Constant-time equality. Slices of different lengths are simply unequal.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, VerifyError> {
    headers
        .get(name)
        .ok_or_else(|| VerifyError::MissingHeader(name.to_string()))?
        .to_str()
        .map_err(|_| VerifyError::Malformed)
}

/// One configured way of checking a source.
#[derive(Debug, Clone)]
pub enum Verifier {
    Github(GithubVerifier),
    Hmac(HmacVerifier),
    Midtrans(MidtransVerifier),
    Xendit(XenditVerifier),
}

impl Verifier {
    /// `now` is unix seconds. Only the timestamped HMAC scheme looks at it.
    pub fn verify(&self, headers: &HeaderMap, body: &[u8], now: i64) -> Result<(), VerifyError> {
        match self {
            Verifier::Github(v) => v.verify(headers, body),
            Verifier::Hmac(v) => v.verify(headers, body, now),
            Verifier::Midtrans(v) => v.verify(body),
            Verifier::Xendit(v) => v.verify(headers),
        }
    }
}

/// What a passed check does and does not prove. The tag goes to targets in
/// `x-inbox-verified`, the text is shown on the delivery page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub tag: &'static str,
    pub text: &'static str,
}

impl Verifier {
    pub fn scope(&self) -> Scope {
        match self {
            Verifier::Github(_) => Scope {
                tag: "body-hmac",
                text: "HMAC-SHA256 over the whole body. Any change to the body fails the check.",
            },
            Verifier::Hmac(_) => Scope {
                tag: "body-hmac",
                text: "HMAC over the whole body. Any change to the body fails the check.",
            },
            Verifier::Midtrans(_) => Scope {
                tag: "midtrans-signed-fields",
                text: "Signature covers order_id, status_code and gross_amount only. \
                       transaction_status, transaction_id and fraud_status are not signed, \
                       so confirm the status with the Midtrans API before acting on it.",
            },
            Verifier::Xendit(_) => Scope {
                tag: "shared-token",
                text: "Shared token in x-callback-token. The body is not signed, so confirm \
                       the status with the Xendit API before acting on it.",
            },
        }
    }
}

/// Key material that prints as `[redacted]`, so a stray `{:?}` or a derived
/// `Debug` on a config struct cannot leak it into logs.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Secret(s.as_bytes().to_vec())
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Secret(s.into_bytes())
    }
}

impl From<Vec<u8>> for Secret {
    fn from(v: Vec<u8>) -> Self {
        Secret(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_hides_secrets() {
        let v = Verifier::Github(GithubVerifier::new("super-secret-value"));
        assert!(!format!("{v:?}").contains("super-secret-value"));
        let m = Verifier::Midtrans(MidtransVerifier::new("super-secret-value"));
        assert!(!format!("{m:?}").contains("super-secret-value"));
    }

    #[test]
    fn scope_says_what_each_check_covers() {
        let m = Verifier::Midtrans(MidtransVerifier::new("k")).scope();
        assert_eq!(m.tag, "midtrans-signed-fields");
        assert!(m.text.contains("order_id") && m.text.contains("not signed"));
        let x = Verifier::Xendit(XenditVerifier::new("k")).scope();
        assert_eq!(x.tag, "shared-token");
        assert!(x.text.contains("body is not signed"));
        assert_eq!(
            Verifier::Github(GithubVerifier::new("k")).scope().tag,
            "body-hmac"
        );
    }

    #[test]
    fn ct_eq_rejects_different_lengths() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
    }
}
