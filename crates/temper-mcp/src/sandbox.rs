//! Monty sandbox utility functions.

use std::time::Duration;

use monty::{MontyException, MontyObject, ResourceLimits};
use reqwest::StatusCode;
use serde_json::{Map, Value};

use super::convert::monty_object_to_json;

pub(super) fn wrap_user_code(code: &str) -> String {
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

pub(super) fn default_limits() -> ResourceLimits {
    // 180s allows time for governance operations like poll_decision,
    // where the agent waits for human approval.
    ResourceLimits::new()
        .max_duration(Duration::from_secs(180))
        .max_memory(64 * 1024 * 1024)
        .max_allocations(250_000)
}

pub(super) fn format_monty_exception(exception: &MontyException) -> String {
    if exception.traceback().is_empty() {
        exception.summary()
    } else {
        exception.to_string()
    }
}

pub(super) fn escape_odata_key(key: &str) -> String {
    key.replace('\'', "''")
}

/// Extract an optional string argument at `index`, returning `None` if absent.
pub(super) fn optional_string_arg(args: &[MontyObject], index: usize) -> Option<String> {
    args.get(index).and_then(|a| String::try_from(a).ok())
}

pub(super) fn expect_string_arg(
    args: &[MontyObject],
    index: usize,
    name: &str,
    method: &str,
) -> std::result::Result<String, String> {
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

pub(super) fn expect_json_object_arg(
    args: &[MontyObject],
    index: usize,
    name: &str,
    method: &str,
) -> std::result::Result<Map<String, Value>, String> {
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

pub(super) fn format_http_error(status: StatusCode, body: &str) -> String {
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
/// Returns a rich error message with decision ID and guidance if the body
/// matches the Temper AuthorizationDenied format. Instructs the agent to
/// wait for human approval via `poll_decision` rather than self-approving.
pub(super) fn format_authz_denied(body: &str) -> Option<String> {
    let json: Value = serde_json::from_str(body).ok()?;
    let code = json.pointer("/error/code").and_then(Value::as_str)?;
    if code != "AuthorizationDenied" {
        return None;
    }

    let message = json
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Authorization denied");

    // Look for decision ID in the message (format: "PD-<uuid>")
    let decision_hint = if let Some(pos) = message.find("PD-") {
        let id_end = message[pos..]
            .find(|c: char| c.is_whitespace() || c == ')' || c == '"')
            .unwrap_or(message.len() - pos);
        let decision_id = &message[pos..pos + id_end];
        format!(
            "\n\nDecision: {decision_id} (pending human approval)\n\
             A human must approve this action in the Observe UI or via `temper decide`.\n\
             Use `await temper.poll_decision(tenant, '{decision_id}')` to wait for the decision.\n\
             Do NOT attempt to approve this yourself — governance write methods are not available."
        )
    } else {
        "\n\nThis action requires human approval. A decision has been created.\n\
         Use `await temper.get_decisions(tenant)` to find the decision ID,\n\
         then `await temper.poll_decision(tenant, decision_id)` to wait for approval."
            .to_string()
    };

    Some(format!("AuthorizationDenied: {message}{decision_hint}"))
}
