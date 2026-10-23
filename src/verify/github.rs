use hmac::{Hmac, KeyInit, Mac};
use http::HeaderMap;
use sha2::Sha256;

use super::{VerifyError, ct_eq, header_str};

const HEADER: &str = "x-hub-signature-256";

/// GitHub: `X-Hub-Signature-256: sha256=<hex hmac of the raw body>`.
#[derive(Debug, Clone)]
pub struct GithubVerifier {
    secret: Vec<u8>,
}

impl GithubVerifier {
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self { secret: secret.into() }
    }

    pub fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<(), VerifyError> {
        let value = header_str(headers, HEADER)?;
        let hex_sig = value.strip_prefix("sha256=").ok_or(VerifyError::Malformed)?;
        let given = hex::decode(hex_sig).map_err(|_| VerifyError::Malformed)?;
        if given.is_empty() {
            return Err(VerifyError::Malformed);
        }

        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).expect("hmac takes any key length");
        mac.update(body);
        let expected = mac.finalize().into_bytes();

        if ct_eq(&expected, &given) {
            Ok(())
        } else {
            Err(VerifyError::Mismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Example from GitHub's "Validating webhook deliveries" page.
    const SECRET: &str = "It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";
    const SIG: &str = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    fn headers(sig: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(HEADER, sig.parse().unwrap());
        h
    }

    #[test]
    fn accepts_the_documented_vector() {
        let v = GithubVerifier::new(SECRET);
        assert_eq!(v.verify(&headers(SIG), BODY), Ok(()));
    }

    #[test]
    fn rejects_a_tampered_body() {
        let v = GithubVerifier::new(SECRET);
        assert_eq!(v.verify(&headers(SIG), b"Hello, World?"), Err(VerifyError::Mismatch));
    }

    #[test]
    fn rejects_the_wrong_secret() {
        let v = GithubVerifier::new("another secret");
        assert_eq!(v.verify(&headers(SIG), BODY), Err(VerifyError::Mismatch));
    }

    #[test]
    fn rejects_a_missing_or_malformed_header() {
        let v = GithubVerifier::new(SECRET);
        assert_eq!(
            v.verify(&HeaderMap::new(), BODY),
            Err(VerifyError::MissingHeader(HEADER.into()))
        );
        assert_eq!(v.verify(&headers("757107ea"), BODY), Err(VerifyError::Malformed));
        assert_eq!(v.verify(&headers("sha256=zz"), BODY), Err(VerifyError::Malformed));
        assert_eq!(v.verify(&headers("sha256="), BODY), Err(VerifyError::Malformed));
    }

    #[test]
    fn rejects_a_truncated_signature() {
        let v = GithubVerifier::new(SECRET);
        let short = &SIG[..SIG.len() - 2];
        assert_eq!(v.verify(&headers(short), BODY), Err(VerifyError::Mismatch));
    }
}
