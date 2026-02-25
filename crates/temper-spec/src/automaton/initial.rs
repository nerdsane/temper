//! Shared parsing helpers for IOA `[[state]] initial` values.

use serde_json::Value;

/// Parse a bool-like initial value.
///
/// Accepted true values: `true`, `1`, `yes`, `on` (case-insensitive).
/// Everything else is false.
pub fn parse_bool_initial(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// Parse a counter-like initial value as `usize`, defaulting to 0.
pub fn parse_counter_initial_usize(raw: &str) -> usize {
    raw.trim().parse::<usize>().unwrap_or(0)
}

/// Parse list/set initial value syntax into strings.
///
/// Supports:
/// - `[]`
/// - `[a, b]`
/// - `["a", "b"]`
/// - `'single'` / `"single"` / `single`
pub fn parse_list_initial(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return Vec::new();
    }

    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_matches('"').trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    vec![trimmed.trim_matches('"').trim_matches('\'').to_string()]
}

/// Parse a typed state initial value to JSON for tooling surfaces (MCP/spec views).
pub fn parse_var_initial_json(var_type: &str, raw: &str) -> Value {
    match var_type {
        "bool" => Value::Bool(parse_bool_initial(raw)),
        "counter" | "int" | "integer" => raw
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_string())),
        "list" | "set" => Value::Array(
            parse_list_initial(raw)
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
        _ => Value::String(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_initial_variants() {
        assert!(parse_bool_initial("true"));
        assert!(parse_bool_initial("YES"));
        assert!(!parse_bool_initial("false"));
        assert!(!parse_bool_initial("0"));
    }

    #[test]
    fn list_initial_variants() {
        assert_eq!(parse_list_initial("[]"), Vec::<String>::new());
        assert_eq!(parse_list_initial("[a,b]"), vec!["a", "b"]);
        assert_eq!(parse_list_initial("[\"a\", \"b\"]"), vec!["a", "b"]);
        assert_eq!(parse_list_initial("'x'"), vec!["x"]);
    }

    #[test]
    fn var_initial_json_variants() {
        assert_eq!(parse_var_initial_json("bool", "true"), Value::Bool(true));
        assert_eq!(parse_var_initial_json("counter", "3"), Value::from(3));
        assert_eq!(
            parse_var_initial_json("list", "[a,b]"),
            serde_json::json!(["a", "b"])
        );
    }
}
