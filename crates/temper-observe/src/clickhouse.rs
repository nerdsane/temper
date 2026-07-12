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
    UnicodeSingleQuoted,
    UnicodeDoubleQuoted,
    LineComment,
    BlockComment(usize),
    Heredoc {
        delimiter_start: usize,
        delimiter_len: usize,
    },
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
                    if c == b'$'
                        && let Some(delimiter_len) = heredoc_delimiter_len(sql, i)
                    {
                        out.push_str(&sql[i..i + delimiter_len]);
                        let delimiter_start = i;
                        i += delimiter_len;
                        region = QueryRegion::Heredoc {
                            delimiter_start,
                            delimiter_len,
                        };
                        continue;
                    }
                    if is_clickhouse_word_char(c) {
                        let mut end = i + 1;
                        while end < bytes.len()
                            && (is_clickhouse_word_char(bytes[end]) || bytes[end] == b'$')
                        {
                            end += 1;
                        }
                        out.push_str(&sql[i..end]);
                        i = end;
                        continue;
                    }
                    let next = bytes.get(i + 1).copied();
                    region = match (c, next) {
                        (b'\'', _) => QueryRegion::SingleQuoted,
                        (b'"', _) => QueryRegion::DoubleQuoted,
                        (b'`', _) => QueryRegion::BacktickQuoted,
                        (b'-', Some(b'-')) | (b'/', Some(b'/')) => QueryRegion::LineComment,
                        (b'#', Some(b' ' | b'!')) => QueryRegion::LineComment,
                        (b'/', Some(b'*')) => {
                            out.push_str("/*");
                            i += 2;
                            region = QueryRegion::BlockComment(1);
                            continue;
                        }
                        (0xE2, Some(0x80)) if bytes.get(i + 2) == Some(&0x98) => {
                            push_char(sql, &mut out, &mut i);
                            region = QueryRegion::UnicodeSingleQuoted;
                            continue;
                        }
                        (0xE2, Some(0x80)) if bytes.get(i + 2) == Some(&0x9C) => {
                            push_char(sql, &mut out, &mut i);
                            region = QueryRegion::UnicodeDoubleQuoted;
                            continue;
                        }
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
                QueryRegion::BlockComment(depth) => {
                    if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        out.push_str("/*");
                        i += 2;
                        region = QueryRegion::BlockComment(depth + 1);
                        continue;
                    }
                    if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        out.push_str("*/");
                        i += 2;
                        region = if depth == 1 {
                            QueryRegion::Code
                        } else {
                            QueryRegion::BlockComment(depth - 1)
                        };
                        continue;
                    }
                }
                QueryRegion::UnicodeSingleQuoted | QueryRegion::UnicodeDoubleQuoted => {
                    let closing = if region == QueryRegion::UnicodeSingleQuoted {
                        "’"
                    } else {
                        "”"
                    };
                    if sql[i..].starts_with(closing) {
                        region = QueryRegion::Code;
                    }
                }
                QueryRegion::Heredoc {
                    delimiter_start,
                    delimiter_len,
                } => {
                    let delimiter = &sql[delimiter_start..delimiter_start + delimiter_len];
                    if sql[i..].starts_with(delimiter) {
                        out.push_str(delimiter);
                        i += delimiter_len;
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

fn heredoc_delimiter_len(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let tag_len = bytes[start + 1..].iter().position(|byte| *byte == b'$')?;
    let delimiter_len = tag_len + 2;
    let tag = &bytes[start + 1..start + 1 + tag_len];
    if !tag
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return None;
    }
    let delimiter = &source[start..start + delimiter_len];
    source[start + delimiter_len..]
        .contains(delimiter)
        .then_some(delimiter_len)
}

fn is_clickhouse_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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
#[path = "clickhouse_tests.rs"]
mod tests;
