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
/// 180s allows time for governance operations like poll_decision,
/// where the agent waits for human approval.
pub fn default_limits() -> ResourceLimits {
    ResourceLimits::new()
        .max_duration(Duration::from_secs(180))
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
