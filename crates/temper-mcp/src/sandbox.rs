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
    ResourceLimits::new()
        .max_duration(Duration::from_secs(2))
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
