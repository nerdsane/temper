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

    fn interpolate_params(sql: &str, params: &[SqlParam]) -> String {
        let mut result = sql.to_string();
        for (i, param) in params.iter().enumerate().rev() {
            let placeholder = format!("${}", i + 1);
            let value = match param {
                SqlParam::String(s) => format!("'{}'", s.replace('\'', "''")),
                SqlParam::Int(i) => i.to_string(),
                SqlParam::Float(f) => f.to_string(),
                SqlParam::Bool(b) => if *b { "1" } else { "0" }.to_string(),
                SqlParam::Null => "NULL".to_string(),
            };
            result = result.replace(&placeholder, &value);
        }
        result
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
