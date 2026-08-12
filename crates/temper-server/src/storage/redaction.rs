//! Secret redaction for captured request bodies.
//!
//! Every dispatch records the parameters it was called with, successful ones
//! included, and those parameters are read back through trajectory observation
//! surfaces and exported into training data. An action that takes an
//! `api_token`, a `password`, or a card number would put that value in a
//! durable, widely-readable row.
//!
//! Redaction runs at the single enqueue choke point
//! (`ServerState::enqueue_trajectory_entry`), before the size cap, so:
//!
//! - every capture site is covered — dispatch, authorization denials, the
//!   audit endpoint, unmet intents — and a new one cannot forget it;
//! - a truncated preview can never contain a value the full body would have
//!   had redacted.
//!
//! # What is redacted
//!
//! The name of the field, not its contents: value-shape guessing produces both
//! misses and false positives, while a field called `password` is a password
//! whatever it holds. A key is redacted when its normalized form — lowercased,
//! with `_`, `-`, `.`, and spaces removed — is one of [`SECRET_KEY_NAMES`] or
//! ends with one of [`SECRET_KEY_SUFFIXES`]. The key stays, so a reader still
//! sees which arguments were passed; only the value becomes
//! [`REDACTED_PLACEHOLDER`].
//!
//! The suffix list is deliberately singular (`token`, not `tokens`) and does
//! not include bare `key`: `prompt_tokens`, `completion_token_ids`, and
//! `idempotency_key` are operational data, and redacting them would corrupt
//! the trajectory to no benefit.
//!
//! This is a name-based net, not a proof. An action that puts a credential in
//! a field named `value` is not caught; catching that needs the spec to
//! declare which parameters are sensitive, which the IOA action grammar does
//! not yet express.

use serde_json::{Map, Value};

/// Value written in place of a redacted field.
pub(crate) const REDACTED_PLACEHOLDER: &str = "[redacted]";

/// Normalized key names whose value is always a secret.
pub(crate) const SECRET_KEY_NAMES: &[&str] = &[
    "accesskey",
    "accountnumber",
    "auth",
    "authorization",
    "cardnumber",
    "credential",
    "credentials",
    "cvc",
    "cvv",
    "iban",
    "passphrase",
    "passwd",
    "password",
    "pin",
    "privatekey",
    "pwd",
    "routingnumber",
    "secret",
    "secretkey",
    "securitycode",
    "sessionkey",
    "ssn",
    "token",
];

/// Normalized key endings whose value is always a secret.
pub(crate) const SECRET_KEY_SUFFIXES: &[&str] = &[
    "accesskey",
    "apikey",
    "cardnumber",
    "credential",
    "credentials",
    "passphrase",
    "passwd",
    "password",
    "privatekey",
    "secret",
    "secretkey",
    "sessionkey",
    "token",
];

/// Replace the value of every secret-named field, at any depth.
pub(crate) fn redact_secrets(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_object(map)),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_secrets).collect()),
        other => other,
    }
}

fn redact_object(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .map(|(key, value)| {
            if is_secret_key(&key) {
                (key, Value::String(REDACTED_PLACEHOLDER.to_string()))
            } else {
                (key, redact_secrets(value))
            }
        })
        .collect()
}

/// Whether a field name names a secret.
fn is_secret_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    SECRET_KEY_NAMES.contains(&normalized.as_str())
        || SECRET_KEY_SUFFIXES
            .iter()
            .any(|suffix| normalized.ends_with(suffix))
}

/// Lowercase and drop the separators that only vary by naming convention, so
/// `apiToken`, `api_token`, `API-TOKEN`, and `api.token` are one name.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| !matches!(c, '_' | '-' | '.' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_named_fields_lose_their_values() {
        let body = serde_json::json!({
            "password": "hunter2",
            "api_token": "sk-live-abc",
            "apiKey": "k-1",
            "Authorization": "Bearer abc",
            "card_number": "4111111111111111",
            "cvv": 123,
            "client_secret": "cs-1",
        });
        let redacted = redact_secrets(body);
        for key in [
            "password",
            "api_token",
            "apiKey",
            "Authorization",
            "card_number",
            "cvv",
            "client_secret",
        ] {
            assert_eq!(
                redacted[key], REDACTED_PLACEHOLDER,
                "`{key}` must not be stored verbatim"
            );
        }
    }

    #[test]
    fn nested_and_arrayed_secrets_are_reached() {
        let body = serde_json::json!({
            "connection": {"host": "db", "password": "hunter2"},
            "accounts": [{"id": "a", "access_token": "t-1"}, {"id": "b"}],
        });
        let redacted = redact_secrets(body);
        assert_eq!(redacted["connection"]["host"], "db");
        assert_eq!(redacted["connection"]["password"], REDACTED_PLACEHOLDER);
        assert_eq!(redacted["accounts"][0]["id"], "a");
        assert_eq!(
            redacted["accounts"][0]["access_token"],
            REDACTED_PLACEHOLDER
        );
    }

    #[test]
    fn a_secret_object_is_replaced_whole_rather_than_walked_into() {
        let body = serde_json::json!({"credentials": {"user": "rita", "password": "hunter2"}});
        let redacted = redact_secrets(body);
        assert_eq!(
            redacted["credentials"], REDACTED_PLACEHOLDER,
            "a secret-named object leaks through its own fields if only its children are redacted"
        );
    }

    #[test]
    fn operational_fields_that_merely_look_secretish_are_kept() {
        let body = serde_json::json!({
            "prompt_tokens": 128,
            "completion_token_ids": [1, 2, 3],
            "idempotency_key": "idem-1",
            "author": "rita",
            "keyboard": "qwerty",
            "monkey": "george",
            "public_key_id": "kid-1",
        });
        let redacted = redact_secrets(body.clone());
        assert_eq!(
            redacted, body,
            "redaction must not corrupt ordinary trajectory data"
        );
    }

    #[test]
    fn a_non_object_body_passes_through() {
        assert_eq!(
            redact_secrets(serde_json::json!("plain string")),
            serde_json::json!("plain string")
        );
        assert_eq!(redact_secrets(Value::Null), Value::Null);
    }
}
