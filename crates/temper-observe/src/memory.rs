//! In-memory observability store for testing.
//!
//! Stores spans, logs, and metrics as in-memory vectors and supports
//! basic SQL-like filtering: either return all rows or filter by equality
//! on a single column via `WHERE column = 'value'` syntax.

use std::sync::{Arc, RwLock};

use serde_json::Value as JsonValue;

use crate::error::ObserveError;
use crate::schema::{LOG_COLUMNS, METRIC_COLUMNS, SPAN_COLUMNS};
use crate::store::{ObservabilityStore, ResultRow, ResultSet, SqlParam};


/// An in-memory row is a map from column name to JSON value.
type Row = Vec<(String, JsonValue)>;

/// In-memory backing storage shared across clones.
#[derive(Debug, Default)]
struct Inner {
    spans: Vec<Row>,
    logs: Vec<Row>,
    metrics: Vec<Row>,
}

/// In-memory implementation of [`ObservabilityStore`] for tests.
///
/// Thread-safe via internal `RwLock`. Clone is cheap (shared `Arc`).
#[derive(Debug, Clone, Default)]
pub struct InMemoryStore {
    inner: Arc<RwLock<Inner>>,
}

impl InMemoryStore {
    /// Create a new, empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    // ---- Insertion helpers ----

    /// Insert a span row. The provided pairs must use column names from
    /// [`SPAN_COLUMNS`].
    pub fn insert_span(&self, columns: Vec<(String, JsonValue)>) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.spans.push(columns);
    }

    /// Insert a log row. The provided pairs must use column names from
    /// [`LOG_COLUMNS`].
    pub fn insert_log(&self, columns: Vec<(String, JsonValue)>) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.logs.push(columns);
    }

    /// Insert a metric row. The provided pairs must use column names from
    /// [`METRIC_COLUMNS`].
    pub fn insert_metric(&self, columns: Vec<(String, JsonValue)>) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.metrics.push(columns);
    }
}

// ---- Minimal SQL-like filter parsing ----

/// A parsed filter: either no filter (return all rows) or an equality check.
#[derive(Debug)]
enum Filter {
    /// Return every row.
    All,
    /// Return rows where `column = value`.
    Eq { column: String, value: String },
}

/// Parse a very minimal SQL-like query.
///
/// Accepted forms:
/// - `SELECT * FROM <table>`                     -> Filter::All
/// - `SELECT * FROM <table> WHERE col = 'val'`   -> Filter::Eq
/// - `SELECT * FROM <table> WHERE col = $1`       -> Filter::Eq (param bind)
///
/// Anything else returns an error.
fn parse_filter(sql: &str, params: &[SqlParam]) -> Result<Filter, ObserveError> {
    let sql = sql.trim();

    // Must start with SELECT
    let upper = sql.to_uppercase();
    if !upper.starts_with("SELECT") {
        return Err(ObserveError::InvalidQuery(
            "query must start with SELECT".into(),
        ));
    }

    // Look for WHERE clause
    if let Some(where_pos) = upper.find("WHERE") {
        let clause = sql[where_pos + 5..].trim();
        // Expect: column = 'value' OR column = $N
        let parts: Vec<&str> = clause.splitn(2, '=').collect();
        if parts.len() != 2 {
            return Err(ObserveError::InvalidQuery(
                "WHERE clause must be `column = 'value'` or `column = $N`".into(),
            ));
        }
        let column = parts[0].trim().to_string();
        let rhs = parts[1].trim();

        let value = if let Some(idx_str) = rhs.strip_prefix('$') {
            // Parameter binding: $1, $2, ...
            let idx: usize = idx_str.parse().map_err(|_| {
                ObserveError::InvalidQuery(format!("invalid parameter index: {rhs}"))
            })?;
            // $1 is params[0]
            if idx == 0 || idx > params.len() {
                return Err(ObserveError::InvalidQuery(format!(
                    "parameter ${idx} out of range (have {} params)",
                    params.len()
                )));
            }
            param_to_string(&params[idx - 1])
        } else if rhs.starts_with('\'') && rhs.ends_with('\'') && rhs.len() >= 2 {
            rhs[1..rhs.len() - 1].to_string()
        } else {
            // Bare value (number, etc.)
            rhs.to_string()
        };

        Ok(Filter::Eq { column, value })
    } else {
        Ok(Filter::All)
    }
}

/// Convert a SqlParam to its string representation for equality matching.
fn param_to_string(param: &SqlParam) -> String {
    match param {
        SqlParam::String(s) => s.clone(),
        SqlParam::Int(i) => i.to_string(),
        SqlParam::Float(f) => f.to_string(),
        SqlParam::Bool(b) => b.to_string(),
        SqlParam::Null => "null".to_string(),
    }
}

/// Apply a filter to a set of rows, returning matching rows as a ResultSet.
fn apply_filter(
    rows: &[Row],
    schema_columns: &[&str],
    filter: &Filter,
) -> Result<ResultSet, ObserveError> {
    let columns: Vec<String> = schema_columns.iter().map(|s| (*s).to_string()).collect();

    let filtered: Vec<ResultRow> = rows
        .iter()
        .filter(|row| match filter {
            Filter::All => true,
            Filter::Eq { column, value } => row
                .iter()
                .find(|(name, _)| name == column)
                .map(|(_, v)| json_value_matches(v, value))
                .unwrap_or(false),
        })
        .map(|row| ResultRow {
            columns: row.clone(),
        })
        .collect();

    Ok(ResultSet {
        columns,
        rows: filtered,
    })
}

/// Check if a JSON value matches a string representation.
fn json_value_matches(value: &JsonValue, target: &str) -> bool {
    match value {
        JsonValue::String(s) => s == target,
        JsonValue::Number(n) => n.to_string() == target,
        JsonValue::Bool(b) => b.to_string() == target,
        JsonValue::Null => target == "null",
        _ => false,
    }
}

impl ObservabilityStore for InMemoryStore {
    async fn query_spans(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<ResultSet, ObserveError> {
        let filter = parse_filter(sql, params)?;
        let inner = self.inner.read().expect("lock poisoned");
        apply_filter(&inner.spans, SPAN_COLUMNS, &filter)
    }

    async fn query_logs(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<ResultSet, ObserveError> {
        let filter = parse_filter(sql, params)?;
        let inner = self.inner.read().expect("lock poisoned");
        apply_filter(&inner.logs, LOG_COLUMNS, &filter)
    }

    async fn query_metrics(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<ResultSet, ObserveError> {
        let filter = parse_filter(sql, params)?;
        let inner = self.inner.read().expect("lock poisoned");
        apply_filter(&inner.metrics, METRIC_COLUMNS, &filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_query_all_spans() {
        let store = InMemoryStore::new();
        store.insert_span(vec![
            ("trace_id".into(), json!("t1")),
            ("span_id".into(), json!("s1")),
            ("service".into(), json!("api")),
            ("operation".into(), json!("GET /users")),
            ("status".into(), json!("ok")),
        ]);
        store.insert_span(vec![
            ("trace_id".into(), json!("t1")),
            ("span_id".into(), json!("s2")),
            ("service".into(), json!("db")),
            ("operation".into(), json!("SELECT")),
            ("status".into(), json!("ok")),
        ]);

        let result = store
            .query_spans("SELECT * FROM spans", &[])
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn test_query_spans_with_where_clause() {
        let store = InMemoryStore::new();
        store.insert_span(vec![
            ("trace_id".into(), json!("t1")),
            ("service".into(), json!("api")),
            ("status".into(), json!("ok")),
        ]);
        store.insert_span(vec![
            ("trace_id".into(), json!("t2")),
            ("service".into(), json!("db")),
            ("status".into(), json!("error")),
        ]);

        let result = store
            .query_spans("SELECT * FROM spans WHERE service = 'api'", &[])
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.rows[0].get("service"),
            Some(&json!("api"))
        );
    }

    #[tokio::test]
    async fn test_query_with_param_binding() {
        let store = InMemoryStore::new();
        store.insert_log(vec![
            ("level".into(), json!("error")),
            ("service".into(), json!("auth")),
            ("message".into(), json!("login failed")),
        ]);
        store.insert_log(vec![
            ("level".into(), json!("info")),
            ("service".into(), json!("auth")),
            ("message".into(), json!("login ok")),
        ]);

        let result = store
            .query_logs(
                "SELECT * FROM logs WHERE level = $1",
                &[SqlParam::String("error".into())],
            )
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.rows[0].get("message"),
            Some(&json!("login failed"))
        );
    }

    #[tokio::test]
    async fn test_query_metrics_empty() {
        let store = InMemoryStore::new();
        let result = store
            .query_metrics("SELECT * FROM metrics", &[])
            .await
            .unwrap();
        assert!(result.is_empty());
        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_insert_and_query_metrics() {
        let store = InMemoryStore::new();
        store.insert_metric(vec![
            ("metric_name".into(), json!("latency_p99")),
            ("timestamp".into(), json!("2025-01-01T00:00:00Z")),
            ("value".into(), json!(42.5)),
            ("tags".into(), json!({"service": "api", "env": "prod"})),
        ]);

        let result = store
            .query_metrics(
                "SELECT * FROM metrics WHERE metric_name = 'latency_p99'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.rows[0].get("value"),
            Some(&json!(42.5))
        );
    }

    #[tokio::test]
    async fn test_invalid_query_returns_error() {
        let store = InMemoryStore::new();
        let result = store.query_spans("DROP TABLE spans", &[]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_result_row_get_missing_column() {
        let row = ResultRow {
            columns: vec![("a".into(), json!(1))],
        };
        assert!(row.get("b").is_none());
        assert_eq!(row.get("a"), Some(&json!(1)));
    }

    #[tokio::test]
    async fn test_result_set_empty_constructor() {
        let rs = ResultSet::empty(vec!["a".into(), "b".into()]);
        assert!(rs.is_empty());
        assert_eq!(rs.columns.len(), 2);
    }
}
