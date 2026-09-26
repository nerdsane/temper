//! Typed readers over `toml::Value` for the lenient core IOA sections.
//!
//! The core sections (`[automaton]`, `[[state]]`, `[[action]]`, ...) accept
//! scalars where a string is expected (`initial = 0`), a single string where a
//! list is expected (`from = "Draft"`), and `"true"`/`"false"` strings where a
//! boolean is expected. These helpers encode that contract in one place.

use super::AutomatonParseError;
use toml::{Table, Value};

/// Render a scalar as the string the spec author wrote.
///
/// Returns `None` for arrays and tables, which have no scalar spelling.
pub(super) fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Integer(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Boolean(b) => Some(b.to_string()),
        Value::Datetime(d) => Some(d.to_string()),
        Value::Array(_) | Value::Table(_) => None,
    }
}

/// Render any value as a string: scalars as written, arrays and tables as TOML.
pub(super) fn any_string(value: &Value) -> String {
    scalar_string(value).unwrap_or_else(|| value.to_string())
}

/// Read a string-valued key, failing if it holds an array or table.
pub(super) fn string(
    table: &Table,
    section: &str,
    key: &str,
) -> Result<Option<String>, AutomatonParseError> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    scalar_string(value)
        .map(Some)
        .ok_or_else(|| AutomatonParseError::Toml(format!("{section}: '{key}' must be a string")))
}

/// Read a list of strings. A single scalar is a one-element list; empty
/// entries are dropped.
pub(super) fn string_list(
    table: &Table,
    section: &str,
    key: &str,
) -> Result<Vec<String>, AutomatonParseError> {
    let Some(value) = table.get(key) else {
        return Ok(Vec::new());
    };
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let s = scalar_string(item).ok_or_else(|| {
            AutomatonParseError::Toml(format!("{section}: '{key}' must be a list of strings"))
        })?;
        if !s.is_empty() {
            out.push(s);
        }
    }
    Ok(out)
}

/// Read a boolean written as `true`/`false` or `"true"`/`"false"`.
///
/// Returns `None` when the key is absent or holds any other value.
pub(super) fn bool_value(table: &Table, key: &str) -> Option<bool> {
    match table.get(key)? {
        Value::Boolean(b) => Some(*b),
        Value::String(s) if s == "true" => Some(true),
        Value::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

/// Read a non-negative integer written as a number or a numeric string.
///
/// Returns `None` when the key is absent or the value is not such an integer.
pub(super) fn unsigned<T: std::str::FromStr>(table: &Table, key: &str) -> Option<T> {
    table.get(key).and_then(scalar_string)?.parse().ok()
}
