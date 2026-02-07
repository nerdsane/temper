//! ClickHouse adapter for the [`ObservabilityStore`] trait.
//!
//! Queries ClickHouse via its HTTP API and maps results into [`ResultSet`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::ObserveError;
use crate::store::{ObservabilityStore, ResultRow, ResultSet, SqlParam};

/// A span record matching the ClickHouse `spans` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanRecord {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub service: String,
    pub operation: String,
    pub status: String,
    pub duration_ns: u64,
    pub start_time: String,
    pub end_time: String,
    pub attributes: String,
}

/// A log record matching the ClickHouse `logs` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    pub timestamp: String,
    pub level: String,
    pub service: String,
    pub message: String,
    pub attributes: String,
}

/// A metric record matching the ClickHouse `metrics` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricRecord {
    pub metric_name: String,
    pub timestamp: String,
    pub value: f64,
    pub tags: String,
}

/// ClickHouse implementation of [`ObservabilityStore`].
pub struct ClickHouseStore {
    base_url: String,
    client: reqwest::Client,
}

impl ClickHouseStore {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
        }
    }

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

    async fn execute_query(&self, sql: &str, params: &[SqlParam]) -> Result<ResultSet, ObserveError> {
        let final_sql = Self::interpolate_params(sql, params);
        let resp = self.client.post(&self.query_url())
            .body(final_sql)
            .send().await
            .map_err(|e| ObserveError::ConnectionError(e.to_string()))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| ObserveError::ConnectionError(e.to_string()))?;

        if !status.is_success() {
            return Err(ObserveError::ProviderError(format!("ClickHouse HTTP {status}: {body}")));
        }

        parse_json_each_row(&body)
    }

    async fn execute_insert(&self, sql: &str) -> Result<(), ObserveError> {
        let resp = self.client.post(&self.query_url())
            .body(sql.to_string())
            .send().await
            .map_err(|e| ObserveError::ConnectionError(e.to_string()))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ObserveError::ProviderError(format!("ClickHouse INSERT failed: {body}")));
        }
        Ok(())
    }

    /// Insert a span into ClickHouse.
    pub async fn insert_span(&self, span: &SpanRecord) -> Result<(), ObserveError> {
        let parent = span.parent_span_id.as_deref()
            .map(|p| format!("'{}'", p.replace('\'', "''")))
            .unwrap_or_else(|| "NULL".into());
        let sql = format!(
            "INSERT INTO spans VALUES ('{}','{}',{},'{}','{}','{}',{},'{}','{}','{}')",
            span.trace_id, span.span_id, parent, span.service, span.operation,
            span.status, span.duration_ns, span.start_time, span.end_time, span.attributes,
        );
        self.execute_insert(&sql).await
    }

    /// Insert a log into ClickHouse.
    pub async fn insert_log(&self, log: &LogRecord) -> Result<(), ObserveError> {
        let sql = format!(
            "INSERT INTO logs VALUES ('{}','{}','{}','{}','{}')",
            log.timestamp, log.level, log.service,
            log.message.replace('\'', "''"), log.attributes,
        );
        self.execute_insert(&sql).await
    }

    /// Insert a metric into ClickHouse.
    pub async fn insert_metric(&self, metric: &MetricRecord) -> Result<(), ObserveError> {
        let sql = format!(
            "INSERT INTO metrics VALUES ('{}','{}',{},'{}')",
            metric.metric_name, metric.timestamp, metric.value, metric.tags,
        );
        self.execute_insert(&sql).await
    }
}

impl ObservabilityStore for ClickHouseStore {
    async fn query_spans(&self, sql: &str, params: &[SqlParam]) -> Result<ResultSet, ObserveError> {
        self.execute_query(sql, params).await
    }

    async fn query_logs(&self, sql: &str, params: &[SqlParam]) -> Result<ResultSet, ObserveError> {
        self.execute_query(sql, params).await
    }

    async fn query_metrics(&self, sql: &str, params: &[SqlParam]) -> Result<ResultSet, ObserveError> {
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
        if line.is_empty() { continue; }

        let obj: HashMap<String, serde_json::Value> =
            serde_json::from_str(line).map_err(ObserveError::SerializationError)?;

        if columns.is_empty() {
            columns = obj.keys().cloned().collect();
            columns.sort();
        }

        let row_columns: Vec<(String, serde_json::Value)> = columns.iter()
            .map(|col| (col.clone(), obj.get(col).cloned().unwrap_or(serde_json::Value::Null)))
            .collect();

        rows.push(ResultRow { columns: row_columns });
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
        assert_eq!(result, "SELECT * FROM spans WHERE service = 'api' AND duration_ns > 1000");
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
        assert_eq!(rs.rows[0].get("service"), Some(&serde_json::json!("api")));
    }

    #[test]
    fn test_parse_multiple_rows() {
        let body = "{\"a\":1}\n{\"a\":2}";
        let rs = parse_json_each_row(body).unwrap();
        assert_eq!(rs.len(), 2);
    }

    #[test]
    fn test_span_record_serialize() {
        let span = SpanRecord {
            trace_id: "t1".into(), span_id: "s1".into(), parent_span_id: None,
            service: "api".into(), operation: "GET".into(), status: "ok".into(),
            duration_ns: 1000, start_time: "2025-01-01T00:00:00Z".into(),
            end_time: "2025-01-01T00:00:01Z".into(), attributes: "{}".into(),
        };
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(json["trace_id"], "t1");
    }
}
