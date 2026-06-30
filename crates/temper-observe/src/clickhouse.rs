//! ClickHouse adapter for the [`ObservabilityStore`] trait.
//!
//! Queries ClickHouse via its HTTP API and maps results into [`ResultSet`].
//! Write path is handled by OTEL SDK + OTLP export; this module is query-only.

use std::collections::HashMap;

use crate::error::ObserveError;
use crate::store::{ObservabilityStore, ResultRow, ResultSet, SqlParam};

/// ClickHouse implementation of [`ObservabilityStore`] (query-only).
pub struct ClickHouseStore {
    base_url: String,
    client: reqwest::Client,
}

impl ClickHouseStore {
    /// Create a new ClickHouse store pointing at the given HTTP API base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Return the configured base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn query_url(&self) -> String {
        format!("{}/?default_format=JSONEachRow", self.base_url)
    }

    /// Substitute `$N` placeholders with rendered parameter values.
    ///
    /// Single pass so a substituted value can never be re-scanned, and
    /// placeholders inside single-quoted string literals are left intact —
    /// a naive global `replace` corrupts both (e.g. `WHERE name = '$1'`
    /// would mangle the literal, and a value containing `$2` would be
    /// rewritten by a later iteration).
    fn interpolate_params(sql: &str, params: &[SqlParam]) -> String {
        let render = |param: &SqlParam| -> String {
            match param {
                SqlParam::String(s) => format!("'{}'", s.replace('\'', "''")),
                SqlParam::Int(i) => i.to_string(),
                SqlParam::Float(f) => f.to_string(),
                SqlParam::Bool(b) => if *b { "1" } else { "0" }.to_string(),
                SqlParam::Null => "NULL".to_string(),
            }
        };

        let mut out = String::with_capacity(sql.len());
        let bytes = sql.as_bytes();
        let mut i = 0;
        let mut in_string = false;
        while i < bytes.len() {
            // Placeholders, quotes, and escapes are all ASCII; any multibyte
            // UTF-8 sequence starts with a byte >= 0x80 and is copied verbatim
            // below, so indexing by byte never splits a character.
            let c = bytes[i];
            if in_string {
                if c == b'\'' {
                    // `''` is an escaped quote inside a literal, not a terminator.
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        out.push_str("''");
                        i += 2;
                        continue;
                    }
                    in_string = false;
                    out.push('\'');
                    i += 1;
                    continue;
                }
                // Fall through to the multibyte-safe char copy below.
            } else if c == b'\'' {
                in_string = true;
                out.push('\'');
                i += 1;
                continue;
            } else if c == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let idx: usize = sql[i + 1..j].parse().unwrap_or(0);
                match idx.checked_sub(1).and_then(|n| params.get(n)) {
                    Some(param) => out.push_str(&render(param)),
                    // Out-of-range placeholder: leave it verbatim so the
                    // query fails loudly at ClickHouse rather than silently
                    // binding the wrong value.
                    None => out.push_str(&sql[i..j]),
                }
                i = j;
                continue;
            }
            // ASCII byte or the leading byte of a multibyte char: copy the
            // whole char so we never emit an invalid UTF-8 fragment.
            let ch = sql[i..]
                .chars()
                .next()
                .expect("byte index on char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }

    async fn execute_query(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<ResultSet, ObserveError> {
        let final_sql = Self::interpolate_params(sql, params);
        let resp = self
            .client
            .post(self.query_url())
            .body(final_sql)
            .send()
            .await
            .map_err(|e| ObserveError::ConnectionError(e.to_string()))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ObserveError::ConnectionError(e.to_string()))?;

        if !status.is_success() {
            return Err(ObserveError::ProviderError(format!(
                "ClickHouse HTTP {status}: {body}"
            )));
        }

        parse_json_each_row(&body)
    }
}

impl ObservabilityStore for ClickHouseStore {
    async fn query_spans(&self, sql: &str, params: &[SqlParam]) -> Result<ResultSet, ObserveError> {
        self.execute_query(sql, params).await
    }

    async fn query_logs(&self, sql: &str, params: &[SqlParam]) -> Result<ResultSet, ObserveError> {
        self.execute_query(sql, params).await
    }

    async fn query_metrics(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<ResultSet, ObserveError> {
        self.execute_query(sql, params).await
    }
}

/// Parse ClickHouse JSONEachRow response into a ResultSet.
fn parse_json_each_row(body: &str) -> Result<ResultSet, ObserveError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(ResultSet::empty(Vec::new()));
    }

    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<ResultRow> = Vec::new();

    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let obj: HashMap<String, serde_json::Value> =
            serde_json::from_str(line).map_err(ObserveError::SerializationError)?;

        if columns.is_empty() {
            columns = obj.keys().cloned().collect();
            columns.sort();
        }

        let values: Vec<serde_json::Value> = columns
            .iter()
            .map(|col| obj.get(col).cloned().unwrap_or(serde_json::Value::Null))
            .collect();

        rows.push(ResultRow { values });
    }

    Ok(ResultSet { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_construction() {
        let store = ClickHouseStore::new("http://localhost:8123");
        assert_eq!(store.base_url(), "http://localhost:8123");
    }

    #[test]
    fn test_interpolate_params() {
        let sql = "SELECT * FROM spans WHERE service = $1 AND duration_ns > $2";
        let params = vec![SqlParam::String("api".into()), SqlParam::Int(1000)];
        let result = ClickHouseStore::interpolate_params(sql, &params);
        assert_eq!(
            result,
            "SELECT * FROM spans WHERE service = 'api' AND duration_ns > 1000"
        );
    }

    #[test]
    fn interpolate_preserves_placeholder_inside_string_literal() {
        // The literal '$1' must survive untouched; only the bare $1 binds.
        let sql = "SELECT * FROM spans WHERE label = '$1' AND service = $1";
        let params = vec![SqlParam::String("api".into())];
        let result = ClickHouseStore::interpolate_params(sql, &params);
        assert_eq!(
            result,
            "SELECT * FROM spans WHERE label = '$1' AND service = 'api'"
        );
    }

    #[test]
    fn interpolate_does_not_rescan_substituted_value() {
        // A value containing "$2" must not be rewritten by a later pass.
        let sql = "SELECT $1, $2";
        let params = vec![SqlParam::String("has $2 inside".into()), SqlParam::Int(7)];
        let result = ClickHouseStore::interpolate_params(sql, &params);
        assert_eq!(result, "SELECT 'has $2 inside', 7");
    }

    #[test]
    fn interpolate_handles_two_digit_and_multibyte() {
        let sql = "SELECT $10, name = $1, note = 'café ☕'";
        let mut params: Vec<SqlParam> = (1..=10).map(SqlParam::Int).collect();
        params[0] = SqlParam::String("first".into());
        let result = ClickHouseStore::interpolate_params(sql, &params);
        assert_eq!(result, "SELECT 10, name = 'first', note = 'café ☕'");
    }

    #[test]
    fn interpolate_escapes_embedded_quotes() {
        let sql = "WHERE name = $1";
        let params = vec![SqlParam::String("O'Brien".into())];
        let result = ClickHouseStore::interpolate_params(sql, &params);
        assert_eq!(result, "WHERE name = 'O''Brien'");
    }

    #[test]
    fn test_parse_empty() {
        let rs = parse_json_each_row("").unwrap();
        assert!(rs.is_empty());
    }

    #[test]
    fn test_parse_single_row() {
        let body = r#"{"service":"api","status":"ok"}"#;
        let rs = parse_json_each_row(body).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs.get(0, "service"), Some(&serde_json::json!("api")));
    }

    #[test]
    fn test_parse_multiple_rows() {
        let body = "{\"a\":1}\n{\"a\":2}";
        let rs = parse_json_each_row(body).unwrap();
        assert_eq!(rs.len(), 2);
    }
}
