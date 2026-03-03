//! Monty sandbox utility functions.

use std::time::Duration;

use monty::{MontyObject, ResourceLimits};
use reqwest::StatusCode;
use serde_json::{Map, Value};

use super::convert::monty_object_to_json;

/// Wrap user Python code into an async function for Monty execution.
pub(crate) fn wrap_user_code(code: &str) -> String {
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
pub(crate) fn default_limits() -> ResourceLimits {
    ResourceLimits::new()
        .max_duration(Duration::from_secs(180))
        .max_memory(64 * 1024 * 1024)
        .max_allocations(250_000)
}

/// Format a Monty exception for display.
pub(crate) fn format_monty_exception(exception: &monty::MontyException) -> String {
    if exception.traceback().is_empty() {
        exception.summary()
    } else {
        exception.to_string()
    }
}

/// Escape single quotes for OData key segments.
pub(crate) fn escape_odata_key(key: &str) -> String {
    key.replace('\'', "''")
}

/// Extract an optional string argument at `index`.
pub(crate) fn optional_string_arg(args: &[MontyObject], index: usize) -> Option<String> {
    args.get(index).and_then(|a| String::try_from(a).ok())
}

/// Extract a required string argument, returning a descriptive error on failure.
pub(crate) fn expect_string_arg(
    args: &[MontyObject],
    index: usize,
    name: &str,
    method: &str,
) -> Result<String, String> {
    let value = args.get(index).ok_or_else(|| {
        format!(
            "{method} missing required argument `{name}` at position {}",
            index + 1
        )
    })?;

    String::try_from(value).map_err(|e| {
        format!(
            "{method} expected `{name}` to be string, got {} ({e})",
            value.type_name()
        )
    })
}

/// Extract a required JSON object argument.
pub(crate) fn expect_json_object_arg(
    args: &[MontyObject],
    index: usize,
    name: &str,
    method: &str,
) -> Result<Map<String, Value>, String> {
    let value = args.get(index).ok_or_else(|| {
        format!(
            "{method} missing required argument `{name}` at position {}",
            index + 1
        )
    })?;

    match monty_object_to_json(value) {
        Value::Object(map) => Ok(map),
        other => Err(format!(
            "{method} expected `{name}` to be a JSON object, got {}",
            other
        )),
    }
}

/// Format an HTTP error response for display.
pub(crate) fn format_http_error(status: StatusCode, body: &str) -> String {
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
pub(crate) fn format_authz_denied(body: &str) -> Option<Value> {
    let json: Value = serde_json::from_str(body).ok()?;
    let code = json.pointer("/error/code").and_then(Value::as_str)?;
    if code != "AuthorizationDenied" {
        return None;
    }

    let message = json
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Authorization denied");

    let decision_id = if let Some(pos) = message.find("PD-") {
        let id_end = message[pos..]
            .find(|c: char| c.is_whitespace() || c == ')' || c == '"')
            .unwrap_or(message.len() - pos);
        Some(message[pos..pos + id_end].to_string())
    } else {
        None
    };

    let hint = decision_id.as_ref().map_or_else(
        || {
            "A human must approve this action. Use `await temper.poll_decision(decision_id)` to wait, then retry.".to_string()
        },
        |did| {
            format!(
                "Call `await temper.poll_decision('{did}')` to wait for approval, then retry."
            )
        },
    );

    Some(serde_json::json!({
        "denied": true,
        "reason": format!("Cedar denied: {message}"),
        "pending_decision": decision_id,
        "poll_hint": hint,
        "status": "authorization_denied",
        "decision_id": decision_id,
    }))
}
