use http::HeaderMap;

use super::{Secret, VerifyError, ct_eq, header_str};

const HEADER: &str = "x-callback-token";

/// Xendit does not sign the body. It sends a static token you copy from the
/// dashboard in `x-callback-token`, so this is a shared-secret check only.
#[derive(Debug, Clone)]
pub struct XenditVerifier {
    token: Secret,
}

impl XenditVerifier {
    pub fn new(token: impl Into<Secret>) -> Self {
        Self {
            token: token.into(),
        }
    }

    pub fn verify(&self, headers: &HeaderMap) -> Result<(), VerifyError> {
        let given = header_str(headers, HEADER)?;
        if ct_eq(self.token.as_bytes(), given.as_bytes()) {
            Ok(())
        } else {
            Err(VerifyError::Mismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(HEADER, token.parse().unwrap());
        h
    }

    #[test]
    fn accepts_the_configured_token() {
        let v = XenditVerifier::new("xnd-callback-demo");
        assert_eq!(v.verify(&headers("xnd-callback-demo")), Ok(()));
    }

    #[test]
    fn rejects_other_tokens_prefixes_and_extensions() {
        let v = XenditVerifier::new("xnd-callback-demo");
        for t in [
            "xnd-callback-dem0",
            "xnd-callback-dem",
            "xnd-callback-demo2",
            "",
            "XND-CALLBACK-DEMO",
        ] {
            assert_eq!(v.verify(&headers(t)), Err(VerifyError::Mismatch), "{t:?}");
        }
    }

    #[test]
    fn rejects_a_missing_header() {
        let v = XenditVerifier::new("xnd-callback-demo");
        assert_eq!(
            v.verify(&HeaderMap::new()),
            Err(VerifyError::MissingHeader(HEADER.into()))
        );
    }
}
