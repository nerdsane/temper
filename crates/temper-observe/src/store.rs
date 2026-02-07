//! ObservabilityStore trait -- the universal query interface.
//!
//! Sentinel actors and Evolution Records speak ONLY this trait.
//! Provider adapters (Logfire, Datadog, etc.) supply concrete implementations.

use serde::{Deserialize, Serialize};

use crate::error::ObserveError;

/// A row in a query result set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultRow {
    /// Column name-value pairs for this row.
    pub columns: Vec<(String, serde_json::Value)>,
}

impl ResultRow {
    /// Look up a value by column name.
    pub fn get(&self, column: &str) -> Option<&serde_json::Value> {
        self.columns
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, value)| value)
    }
}

/// Query result set returned by store operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultSet {
    /// Ordered column names.
    pub columns: Vec<String>,
    /// Result rows.
    pub rows: Vec<ResultRow>,
}

impl ResultSet {
    /// Create an empty result set with the given column names.
    pub fn empty(columns: Vec<String>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    /// Return the number of rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Return whether the result set is empty.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// SQL parameter for queries.
#[derive(Debug, Clone)]
pub enum SqlParam {
    /// A string value.
    String(String),
    /// A 64-bit integer value.
    Int(i64),
    /// A 64-bit float value.
    Float(f64),
    /// A boolean value.
    Bool(bool),
    /// SQL NULL.
    Null,
}

/// The universal observability query interface.
///
/// Sentinel actors and Evolution Records speak ONLY this.
/// Provider adapters (Logfire, Datadog, etc.) implement this trait.
pub trait ObservabilityStore: Send + Sync + 'static {
    /// Query the spans virtual table.
    fn query_spans(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> impl std::future::Future<Output = Result<ResultSet, ObserveError>> + Send;

    /// Query the logs virtual table.
    fn query_logs(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> impl std::future::Future<Output = Result<ResultSet, ObserveError>> + Send;

    /// Query the metrics virtual table.
    fn query_metrics(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> impl std::future::Future<Output = Result<ResultSet, ObserveError>> + Send;
}
