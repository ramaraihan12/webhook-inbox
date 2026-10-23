use serde_json::Value;
use sha2::{Digest, Sha512};

use super::{VerifyError, ct_eq};

/// Midtrans HTTP notifications carry their own signature in the body:
/// `signature_key = sha512(order_id + status_code + gross_amount + server_key)`,
/// as lowercase hex. All three fields are strings in Midtrans payloads, and
/// they are hashed exactly as sent (`"10000.00"`, not `10000`).
#[derive(Debug, Clone)]
pub struct MidtransVerifier {
    server_key: String,
}

impl MidtransVerifier {
    pub fn new(server_key: impl Into<String>) -> Self {
        Self {
            server_key: server_key.into(),
        }
    }

    pub fn verify(&self, body: &[u8]) -> Result<(), VerifyError> {
        let json: Value = serde_json::from_slice(body).map_err(|_| VerifyError::Malformed)?;
        let field = |name: &str| {
            json.get(name)
                .and_then(Value::as_str)
                .ok_or(VerifyError::Malformed)
        };

        let order_id = field("order_id")?;
        let status_code = field("status_code")?;
        let gross_amount = field("gross_amount")?;
        let given = field("signature_key")?.to_ascii_lowercase();

        let mut hasher = Sha512::new();
        hasher.update(order_id.as_bytes());
        hasher.update(status_code.as_bytes());
        hasher.update(gross_amount.as_bytes());
        hasher.update(self.server_key.as_bytes());
        let expected = hex::encode(hasher.finalize());

        if ct_eq(expected.as_bytes(), given.as_bytes()) {
            Ok(())
        } else {
            Err(VerifyError::Mismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "SB-Mid-server-kabut-demo";
    // sha512("ORD-20261023-001" + "200" + "150000.00" + KEY), computed with Python.
    const SIG: &str = "a88eedbb1d59255d989a19d94ebfe72d1f9978ada3eed7608ac72f00e52ad2394601c9976b4808587c2aa627f71b356ee232b6498e9d68958a8f3b0b18729ef0";

    fn body(order: &str, status: &str, amount: &str, sig: &str) -> Vec<u8> {
        format!(
            r#"{{"order_id":"{order}","status_code":"{status}","gross_amount":"{amount}","transaction_status":"settlement","signature_key":"{sig}"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn accepts_a_known_vector() {
        let v = MidtransVerifier::new(KEY);
        assert_eq!(
            v.verify(&body("ORD-20261023-001", "200", "150000.00", SIG)),
            Ok(())
        );
    }

    #[test]
    fn accepts_an_uppercase_signature() {
        let v = MidtransVerifier::new(KEY);
        let upper = SIG.to_ascii_uppercase();
        assert_eq!(
            v.verify(&body("ORD-20261023-001", "200", "150000.00", &upper)),
            Ok(())
        );
    }

    #[test]
    fn rejects_any_changed_field() {
        let v = MidtransVerifier::new(KEY);
        for b in [
            body("ORD-20261023-002", "200", "150000.00", SIG),
            body("ORD-20261023-001", "201", "150000.00", SIG),
            body("ORD-20261023-001", "200", "150000.01", SIG),
            body("ORD-20261023-001", "200", "150000", SIG),
        ] {
            assert_eq!(v.verify(&b), Err(VerifyError::Mismatch));
        }
    }

    #[test]
    fn rejects_the_wrong_server_key() {
        let v = MidtransVerifier::new("SB-Mid-server-other");
        assert_eq!(
            v.verify(&body("ORD-20261023-001", "200", "150000.00", SIG)),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn rejects_bodies_it_cannot_read() {
        let v = MidtransVerifier::new(KEY);
        assert_eq!(v.verify(b"not json"), Err(VerifyError::Malformed));
        assert_eq!(
            v.verify(br#"{"order_id":"x"}"#),
            Err(VerifyError::Malformed)
        );
        // A numeric gross_amount is not what Midtrans sends and is not guessed at.
        let numeric = format!(
            r#"{{"order_id":"a","status_code":"200","gross_amount":150000,"signature_key":"{SIG}"}}"#
        );
        assert_eq!(v.verify(numeric.as_bytes()), Err(VerifyError::Malformed));
    }
}
