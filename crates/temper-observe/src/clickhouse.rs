//! ClickHouse adapter for the [`ObservabilityStore`] trait.
//!
//! Queries ClickHouse via its HTTP API and maps results into [`ResultSet`].
//! Write path is handled by OTEL SDK + OTLP export; this module is query-only.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::error::ObserveError;
use crate::store::{ObservabilityStore, ResultRow, ResultSet, SqlParam};

/// ClickHouse implementation of [`ObservabilityStore`] (query-only).
pub struct ClickHouseStore {
    base_url: String,
    client: reqwest::Client,
}

#[derive(Debug, PartialEq, Eq)]
struct BoundClickHouseQuery {
    sql: String,
    form_params: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryRegion {
    Code,
    SingleQuoted,
    DoubleQuoted,
    BacktickQuoted,
    LineComment,
    BlockComment,
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
        // ClickHouse resolves `default_format` before it parses a multipart
        // request body. Keep the SQL and parameter values in multipart fields,
        // but select the response format in the URL where the HTTP handler can
        // see it before query execution.
        format!(
            "{}/?default_format=JSONEachRow",
            self.base_url.trim_end_matches('/')
        )
    }

    /// Translate store-wide `$N` placeholders to ClickHouse's typed query
    /// parameters. Values are returned separately for multipart form encoding
    /// and never become part of the SQL source text.
    fn bind_params(sql: &str, params: &[SqlParam]) -> Result<BoundClickHouseQuery, ObserveError> {
        let mut out = String::with_capacity(sql.len());
        let mut used = vec![false; params.len()];
        let bytes = sql.as_bytes();
        let mut i = 0;
        let mut region = QueryRegion::Code;

        while i < bytes.len() {
            let c = bytes[i];

            match region {
                QueryRegion::Code => {
                    let next = bytes.get(i + 1).copied();
                    region = match (c, next) {
                        (b'\'', _) => QueryRegion::SingleQuoted,
                        (b'"', _) => QueryRegion::DoubleQuoted,
                        (b'`', _) => QueryRegion::BacktickQuoted,
                        (b'-', Some(b'-')) | (b'#', _) => QueryRegion::LineComment,
                        (b'/', Some(b'*')) => QueryRegion::BlockComment,
                        (b'$', Some(digit)) if digit.is_ascii_digit() => {
                            let mut end = i + 1;
                            while end < bytes.len() && bytes[end].is_ascii_digit() {
                                end += 1;
                            }
                            let one_based = sql[i + 1..end].parse::<usize>().map_err(|_| {
                                ObserveError::InvalidQuery(format!(
                                    "invalid parameter placeholder {}",
                                    &sql[i..end]
                                ))
                            })?;
                            let index = one_based.checked_sub(1).ok_or_else(|| {
                                ObserveError::InvalidQuery(
                                    "parameter placeholders start at $1".to_string(),
                                )
                            })?;
                            let param = params.get(index).ok_or_else(|| {
                                ObserveError::InvalidQuery(format!(
                                    "parameter ${one_based} was not provided"
                                ))
                            })?;
                            used[index] = true;
                            if let Some((parameter_type, _)) = clickhouse_param(param) {
                                write!(out, "{{p{one_based}:{parameter_type}}}")
                                    .expect("writing to a String cannot fail");
                            } else {
                                out.push_str("NULL");
                            }
                            i = end;
                            continue;
                        }
                        _ => QueryRegion::Code,
                    };
                }
                QueryRegion::SingleQuoted
                | QueryRegion::DoubleQuoted
                | QueryRegion::BacktickQuoted => {
                    let quote = match region {
                        QueryRegion::SingleQuoted => b'\'',
                        QueryRegion::DoubleQuoted => b'"',
                        QueryRegion::BacktickQuoted => b'`',
                        _ => unreachable!(),
                    };
                    if c == b'\\' {
                        push_char(sql, &mut out, &mut i);
                        if i < bytes.len() {
                            push_char(sql, &mut out, &mut i);
                        }
                        continue;
                    }
                    if c == quote {
                        if bytes.get(i + 1) == Some(&quote) {
                            out.push(quote as char);
                            out.push(quote as char);
                            i += 2;
                            continue;
                        }
                        region = QueryRegion::Code;
                    }
                }
                QueryRegion::LineComment => {
                    if c == b'\n' {
                        region = QueryRegion::Code;
                    }
                }
                QueryRegion::BlockComment => {
                    if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        out.push_str("*/");
                        i += 2;
                        region = QueryRegion::Code;
                        continue;
                    }
                }
            }

            push_char(sql, &mut out, &mut i);
        }

        let form_params = used
            .into_iter()
            .zip(params)
            .enumerate()
            .filter_map(|(index, (is_used, param))| {
                if !is_used {
                    return None;
                }
                let (_, value) = clickhouse_param(param)?;
                Some((format!("param_p{}", index + 1), value))
            })
            .collect();

        Ok(BoundClickHouseQuery {
            sql: out,
            form_params,
        })
    }

    fn build_request(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<reqwest::Request, ObserveError> {
        let bound = Self::bind_params(sql, params)?;
        let mut form = reqwest::multipart::Form::new().text("query", bound.sql);
        for (name, value) in bound.form_params {
            form = form.text(name, value);
        }

        self.client
            .post(self.query_url())
            .multipart(form)
            .build()
            .map_err(|e| ObserveError::ConnectionError(e.to_string()))
    }

    async fn execute_query(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<ResultSet, ObserveError> {
        let request = self.build_request(sql, params)?;
        let resp = self
            .client
            .execute(request)
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

fn clickhouse_param(param: &SqlParam) -> Option<(&'static str, String)> {
    match param {
        SqlParam::String(value) => Some(("String", value.clone())),
        SqlParam::Int(value) => Some(("Int64", value.to_string())),
        SqlParam::Float(value) => Some(("Float64", value.to_string())),
        SqlParam::Bool(value) => Some(("UInt8", u8::from(*value).to_string())),
        SqlParam::Null => None,
    }
}

fn push_char(source: &str, output: &mut String, index: &mut usize) {
    let ch = source[*index..]
        .chars()
        .next()
        .expect("index must be on a character boundary");
    output.push(ch);
    *index += ch.len_utf8();
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
    fn bind_params_uses_clickhouse_typed_placeholders() {
        let sql = "SELECT * FROM spans WHERE service = $1 AND duration_ns > $2";
        let params = vec![SqlParam::String("api".into()), SqlParam::Int(1000)];
        let result = ClickHouseStore::bind_params(sql, &params).unwrap();
        assert_eq!(
            result,
            BoundClickHouseQuery {
                sql: "SELECT * FROM spans WHERE service = {p1:String} AND duration_ns > {p2:Int64}"
                    .into(),
                form_params: vec![
                    ("param_p1".into(), "api".into()),
                    ("param_p2".into(), "1000".into()),
                ],
            }
        );
    }

    #[test]
    fn bind_params_preserves_placeholders_in_quoted_regions_and_comments() {
        let sql = "SELECT '$1', \"$1\", `$1`, $1 -- $1\n/* $1 */ # $1\n";
        let params = vec![SqlParam::String("api".into())];
        let result = ClickHouseStore::bind_params(sql, &params).unwrap();
        assert_eq!(
            result.sql,
            "SELECT '$1', \"$1\", `$1`, {p1:String} -- $1\n/* $1 */ # $1\n"
        );
    }

    #[test]
    fn bind_params_never_places_attacker_text_in_sql() {
        let attack = "x\\' OR 1=1; DROP TABLE spans; -- $2";
        let result = ClickHouseStore::bind_params(
            "SELECT * FROM spans WHERE a = $1 AND b = $2",
            &[
                SqlParam::String(attack.into()),
                SqlParam::String("second".into()),
            ],
        )
        .unwrap();

        assert_eq!(
            result.sql,
            "SELECT * FROM spans WHERE a = {p1:String} AND b = {p2:String}"
        );
        assert!(!result.sql.contains("DROP TABLE"));
        assert_eq!(result.form_params[0].1, attack);
    }

    #[test]
    fn bind_params_handles_types_two_digits_and_multibyte_text() {
        let sql = "SELECT $10, name = $1, note = 'café ☕'";
        let mut params: Vec<SqlParam> = (1..=10).map(SqlParam::Int).collect();
        params[0] = SqlParam::String("first".into());
        let result = ClickHouseStore::bind_params(sql, &params).unwrap();
        assert_eq!(
            result.sql,
            "SELECT {p10:Int64}, name = {p1:String}, note = 'café ☕'"
        );
        assert_eq!(
            result.form_params,
            vec![
                ("param_p1".into(), "first".into()),
                ("param_p10".into(), "10".into()),
            ]
        );
    }

    #[test]
    fn bind_params_handles_null_bool_and_float() {
        let result = ClickHouseStore::bind_params(
            "SELECT $1, $2, $3",
            &[SqlParam::Null, SqlParam::Bool(true), SqlParam::Float(1.5)],
        )
        .unwrap();
        assert_eq!(result.sql, "SELECT NULL, {p2:UInt8}, {p3:Float64}");
        assert_eq!(
            result.form_params,
            vec![
                ("param_p2".into(), "1".into()),
                ("param_p3".into(), "1.5".into()),
            ]
        );
    }

    #[test]
    fn bind_params_rejects_missing_and_zero_parameters() {
        let missing = ClickHouseStore::bind_params("SELECT $2", &[SqlParam::Int(1)]);
        assert!(matches!(missing, Err(ObserveError::InvalidQuery(_))));

        let zero = ClickHouseStore::bind_params("SELECT $0", &[SqlParam::Int(1)]);
        assert!(matches!(zero, Err(ObserveError::InvalidQuery(_))));
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

    #[test]
    fn build_request_keeps_parameters_out_of_url() {
        let attack = "x\\' OR 1=1 --";
        let store = ClickHouseStore::new("http://127.0.0.1:8123");
        let request = store
            .build_request(
                "SELECT service FROM spans WHERE service = $1",
                &[SqlParam::String(attack.into())],
            )
            .unwrap();

        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.url().path(), "/");
        assert_eq!(request.url().query(), Some("default_format=JSONEachRow"));
        assert!(!request.url().as_str().contains(attack));
        assert!(
            request
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("multipart/form-data; boundary="))
        );
    }
}
