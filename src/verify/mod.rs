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
