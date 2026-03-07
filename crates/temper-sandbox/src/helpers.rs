//! Monty sandbox utility functions.

use std::time::Duration;

use monty::{MontyException, MontyObject, ResourceLimits};
use reqwest::StatusCode;
use serde_json::{Map, Value};

use crate::convert::monty_object_to_json;

/// Wrap user Python code into an async function for Monty execution.
pub fn wrap_user_code(code: &str) -> String {
    let mut out = String::from("async def __temper_user():\n");

    if code.trim().is_empty() {
        out.push_str("    return None\n");
    } else {
        for line in code.lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push_str("\nawait __temper_user()\n");
    out
}

/// Default resource limits for sandbox execution.
///
/// 30 minutes allows agents to run complex multi-step workflows
/// including governance waits (poll_decision) where the agent
/// blocks on human approval.
pub fn default_limits() -> ResourceLimits {
    ResourceLimits::new()
        .max_duration(Duration::from_secs(1800))
        .max_memory(64 * 1024 * 1024)
        .max_allocations(250_000)
}

/// Format a Monty exception for display.
pub fn format_monty_exception(exception: &MontyException) -> String {
    if exception.traceback().is_empty() {
        exception.summary()
    } else {
        exception.to_string()
    }
}

/// Escape single quotes for OData key segments.
pub fn escape_odata_key(key: &str) -> String {
    key.replace('\'', "''")
}

/// Extract an optional string argument at `index`, returning `None` if absent.
pub fn optional_string_arg(args: &[MontyObject], index: usize) -> Option<String> {
    args.get(index).and_then(|a| String::try_from(a).ok())
}

/// Extract a required string argument, returning a descriptive error on failure.
pub fn expect_string_arg(
    args: &[MontyObject],
    index: usize,
    name: &str,
    method: &str,
) -> Result<String, String> {
    let value = args.get(index).ok_or_else(|| {
        format!(
            "temper.{method} missing required argument `{name}` at position {}",
            index + 1
        )
    })?;

    String::try_from(value).map_err(|e| {
        format!(
            "temper.{method} expected `{name}` to be string, got {} ({e})",
            value.type_name()
        )
    })
}

/// Extract a required JSON object argument.
pub fn expect_json_object_arg(
    args: &[MontyObject],
    index: usize,
    name: &str,
    method: &str,
) -> Result<Map<String, Value>, String> {
    let value = args.get(index).ok_or_else(|| {
        format!(
            "temper.{method} missing required argument `{name}` at position {}",
            index + 1
        )
    })?;

    match monty_object_to_json(value) {
        Value::Object(map) => Ok(map),
        other => Err(format!(
            "temper.{method} expected `{name}` to be a JSON object, got {}",
            other
        )),
    }
}

/// Format an HTTP error response for display.
pub fn format_http_error(status: StatusCode, body: &str) -> String {
    let details = if body.trim().is_empty() {
        "<empty body>".to_string()
    } else if let Ok(json) = serde_json::from_str::<Value>(body) {
        json.pointer("/error/message")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| json.to_string())
    } else {
        body.to_string()
    };

    format!(
        "HTTP {} {}: {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        details
    )
}

/// Parse a 403 Forbidden body for structured Cedar denial information.
///
/// Returns a structured JSON value with decision ID and guidance if the body
/// matches the Temper AuthorizationDenied format.
pub fn format_authz_denied(body: &str) -> Option<Value> {
    let json: Value = serde_json::from_str(body).ok()?;
    let code = json.pointer("/error/code").and_then(Value::as_str)?;
    if code != "AuthorizationDenied" {
        return None;
    }

    let message = json
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Authorization denied");

    // Extract decision ID from the message (format: "PD-<uuid>")
    let decision_id = if let Some(pos) = message.find("PD-") {
        let id_end = message[pos..]
            .find(|c: char| c.is_whitespace() || c == ')' || c == '"')
            .unwrap_or(message.len() - pos);
        Some(message[pos..pos + id_end].to_string())
    } else {
        None
    };

    let default_hint = "A human must approve this action. Use `await temper.poll_decision(tenant, decision_id)` to wait, then retry.".to_string();
    let hint = decision_id.as_ref().map_or(default_hint.clone(), |did| {
        format!(
            "Call `await temper.poll_decision(tenant, '{}')` to wait for approval, then retry.",
            did
        )
    });

    let mut result = serde_json::json!({
        "denied": true,
        "reason": format!("Cedar denied: {message}"),
        "pending_decision": decision_id,
        "poll_hint": hint,
        // Backward-compatible fields
        "status": "authorization_denied",
        "message": format!("Cedar denied: {message}"),
        "observe_url": "http://localhost:3001/decisions",
        "hint": hint
    });

    // Also set decision_id for backward compat
    if let Some(ref did) = decision_id {
        result["decision_id"] = Value::String(did.clone());
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monty::MontyObject;
    use reqwest::StatusCode;
    use serde_json::json;

    #[test]
    fn wrap_user_code_basic() {
        let result = wrap_user_code("x = 1");
        assert!(result.contains("async def __temper_user():"));
        assert!(result.contains("    x = 1"));
    }

    #[test]
    fn wrap_user_code_empty() {
        let result = wrap_user_code("");
        assert!(result.contains("return None"));
    }

    #[test]
    fn wrap_user_code_multiline() {
        let result = wrap_user_code("a = 1\nb = 2");
        assert!(result.contains("    a = 1\n    b = 2"));
    }

    #[test]
    fn escape_odata_key_no_quotes() {
        assert_eq!(escape_odata_key("hello"), "hello");
    }

    #[test]
    fn escape_odata_key_with_quotes() {
        assert_eq!(escape_odata_key("it's"), "it''s");
    }

    #[test]
    fn optional_string_arg_present() {
        let args = vec![MontyObject::String("val".to_string())];
        assert_eq!(optional_string_arg(&args, 0), Some("val".to_string()));
    }

    #[test]
    fn optional_string_arg_absent() {
        let args: Vec<MontyObject> = vec![];
        assert_eq!(optional_string_arg(&args, 0), None);
    }

    #[test]
    fn expect_string_arg_success() {
        let args = vec![MontyObject::String("val".to_string())];
        let result = expect_string_arg(&args, 0, "name", "test_method");
        assert_eq!(result.unwrap(), "val");
    }

    #[test]
    fn expect_string_arg_missing() {
        let args: Vec<MontyObject> = vec![];
        let result = expect_string_arg(&args, 0, "name", "test_method");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required argument"));
    }

    #[test]
    fn expect_string_arg_wrong_type() {
        let args = vec![MontyObject::Int(42)];
        let result = expect_string_arg(&args, 0, "name", "test_method");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expected"));
    }

    #[test]
    fn format_http_error_with_json_body() {
        let body = r#"{"error":{"message":"Not found"}}"#;
        let result = format_http_error(StatusCode::NOT_FOUND, body);
        assert!(result.contains("Not found"));
    }

    #[test]
    fn format_http_error_empty_body() {
        let result = format_http_error(StatusCode::INTERNAL_SERVER_ERROR, "");
        assert!(result.contains("<empty body>"));
    }

    #[test]
    fn format_http_error_plain_text() {
        let result = format_http_error(StatusCode::BAD_REQUEST, "bad request");
        assert!(result.contains("bad request"));
    }

    #[test]
    fn format_authz_denied_valid() {
        let body = r#"{"error":{"code":"AuthorizationDenied","message":"Cedar denied (PD-abc123)"}}"#;
        let result = format_authz_denied(body);
        assert!(result.is_some());
        let val = result.unwrap();
        assert_eq!(val["denied"], json!(true));
        assert_eq!(val["decision_id"], json!("PD-abc123"));
    }

    #[test]
    fn format_authz_denied_not_authz() {
        let body = r#"{"error":{"code":"Other"}}"#;
        let result = format_authz_denied(body);
        assert!(result.is_none());
    }
}
