//! Process-wide redaction for configured credentials.
//!
//! Secret values are registered once at startup and removed before tool text
//! reaches persistence, the model, spool files or live UI events. Structured
//! fields with sensitive names are redacted even when their value was never
//! registered (for example a config file printed by `cat`).

use std::sync::{OnceLock, RwLock};

use serde_json::{Map, Value};

const REDACTED: &str = "[redacted]";
const MIN_SECRET_LEN: usize = 8;

fn secrets() -> &'static RwLock<Vec<String>> {
    static SECRETS: OnceLock<RwLock<Vec<String>>> = OnceLock::new();
    SECRETS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register startup credentials. Short values are ignored to avoid replacing
/// ordinary words; real provider keys are longer than this threshold.
pub fn register_secret(value: impl Into<String>) {
    let value = value.into();
    if value.len() < MIN_SECRET_LEN || value == REDACTED {
        return;
    }
    let mut secrets = secrets().write().unwrap_or_else(|e| e.into_inner());
    if !secrets.iter().any(|known| known == &value) {
        secrets.push(value);
        secrets.sort_by_key(|known| std::cmp::Reverse(known.len()));
    }
}

/// Remove registered values from text, preserving the surrounding JSON.
pub fn redact_text(text: &str) -> String {
    let secrets = secrets().read().unwrap_or_else(|e| e.into_inner());
    let mut output = text.to_string();
    for secret in secrets.iter() {
        if output.contains(secret) {
            output = output.replace(secret, REDACTED);
        }
    }
    output
}

/// Redact registered strings and sensitive object fields at any depth.
pub fn redact_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_text(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        Value::Object(object) => Value::Object(redact_object(object)),
        other => other,
    }
}

fn redact_object(object: Map<String, Value>) -> Map<String, Value> {
    object
        .into_iter()
        .map(|(key, value)| {
            let value = if sensitive_key(&key) {
                Value::String(REDACTED.to_string())
            } else {
                redact_value(value)
            };
            (key, value)
        })
        .collect()
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "api_key" | "apikey" | "authorization" | "password" | "secret" | "token"
    ) || key.ends_with("_api_key")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_password")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registered_value_is_removed_from_plain_and_json_text() {
        let secret = "test-secret-value-123456";
        register_secret(secret);
        let text = format!(r#"{{"api_key":"{secret}","value":42}}"#);
        let redacted = redact_text(&text);
        assert!(!redacted.contains(secret));
        assert!(redacted.contains("[redacted]"));
        assert!(redacted.contains("\"value\":42"));
    }

    #[test]
    fn sensitive_fields_are_redacted_recursively_without_registration() {
        let value = json!({
            "endpoints": {"local": {"api_key": "unregistered-key", "base_url": "https://example.test"}},
            "items": [{"password": "hunter2", "keep": 7}]
        });
        let redacted = redact_value(value);
        assert_eq!(redacted["endpoints"]["local"]["api_key"], REDACTED);
        assert_eq!(redacted["items"][0]["password"], REDACTED);
        assert_eq!(redacted["items"][0]["keep"], 7);
        assert_eq!(redacted["endpoints"]["local"]["base_url"], "https://example.test");
    }

    #[test]
    fn short_values_are_not_registered() {
        register_secret("abc");
        assert_eq!(redact_text("abc and abc"), "abc and abc");
    }
}
