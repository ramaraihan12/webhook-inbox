//! Pulling the provider's own event id out of a delivery, for dedupe.

use http::HeaderMap;
use serde_json::Value;

/// Longer ids are treated as absent rather than truncated, so two different
/// ids can never collapse into one.
const MAX_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventIdSource {
    /// A header such as `x-github-delivery`.
    Header(String),
    /// JSON pointers into the body. With several, the values are joined with `:`
    /// (Midtrans resends the same transaction with a new status, so the id is
    /// the transaction id plus the status).
    JsonPointers(Vec<String>),
}

impl EventIdSource {
    pub fn extract(&self, headers: &HeaderMap, body: &[u8]) -> Option<String> {
        let id = match self {
            EventIdSource::Header(name) => headers.get(name)?.to_str().ok()?.trim().to_string(),
            EventIdSource::JsonPointers(pointers) => {
                let json: Value = serde_json::from_slice(body).ok()?;
                let mut parts = Vec::with_capacity(pointers.len());
                for pointer in pointers {
                    parts.push(scalar(json.pointer(pointer)?)?);
                }
                parts.join(":")
            }
        };
        (!id.is_empty() && id.len() <= MAX_LEN).then_some(id)
    }
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(name: &'static str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, value.parse().unwrap());
        h
    }

    #[test]
    fn reads_a_header() {
        let src = EventIdSource::Header("x-github-delivery".into());
        let h = headers("x-github-delivery", " 72d3162e-cc78 ");
        assert_eq!(src.extract(&h, b"{}").as_deref(), Some("72d3162e-cc78"));
        assert_eq!(src.extract(&HeaderMap::new(), b"{}"), None);
    }

    #[test]
    fn reads_a_json_pointer() {
        let src = EventIdSource::JsonPointers(vec!["/data/id".into()]);
        let body = br#"{"data":{"id":"evt_42"}}"#;
        assert_eq!(
            src.extract(&HeaderMap::new(), body).as_deref(),
            Some("evt_42")
        );
        let numeric = br#"{"data":{"id":42}}"#;
        assert_eq!(
            src.extract(&HeaderMap::new(), numeric).as_deref(),
            Some("42")
        );
    }

    #[test]
    fn joins_several_pointers() {
        let src = EventIdSource::JsonPointers(vec![
            "/transaction_id".into(),
            "/transaction_status".into(),
        ]);
        let body = br#"{"transaction_id":"t-1","transaction_status":"settlement"}"#;
        assert_eq!(
            src.extract(&HeaderMap::new(), body).as_deref(),
            Some("t-1:settlement")
        );
        // One part missing means no id at all, not a half id.
        let partial = br#"{"transaction_id":"t-1"}"#;
        assert_eq!(src.extract(&HeaderMap::new(), partial), None);
    }

    #[test]
    fn gives_up_on_odd_values() {
        let src = EventIdSource::JsonPointers(vec!["/id".into()]);
        let h = HeaderMap::new();
        assert_eq!(src.extract(&h, br#"{"id":null}"#), None);
        assert_eq!(src.extract(&h, br#"{"id":{"a":1}}"#), None);
        assert_eq!(src.extract(&h, br#"{"id":""}"#), None);
        assert_eq!(src.extract(&h, b"not json"), None);
        let long = format!(r#"{{"id":"{}"}}"#, "x".repeat(300));
        assert_eq!(src.extract(&h, long.as_bytes()), None);
    }
}
