//! Canonical observability virtual-table schemas.
//!
//! Every provider adapter maps its native storage into these canonical tables.
//! Sentinel actors and Evolution Records always query against these schemas,
//! ensuring provider-independence.

/// Column names for the canonical `spans` virtual table.
///
/// | Column           | Type      | Description                              |
/// |------------------|-----------|------------------------------------------|
/// | trace_id         | TEXT      | Distributed trace identifier             |
/// | span_id          | TEXT      | Unique span identifier                   |
/// | parent_span_id   | TEXT      | Parent span (NULL for root spans)        |
/// | service          | TEXT      | Originating service name                 |
/// | operation        | TEXT      | Operation / span name                    |
/// | status           | TEXT      | ok, error, unset                         |
/// | duration_ns      | BIGINT    | Duration in nanoseconds                  |
/// | start_time       | TIMESTAMP | Span start time (UTC)                    |
/// | end_time         | TIMESTAMP | Span end time (UTC)                      |
/// | attributes       | JSONB     | Arbitrary key-value attributes           |
pub const SPAN_COLUMNS: &[&str] = &[
    "trace_id",
    "span_id",
    "parent_span_id",
    "service",
    "operation",
    "status",
    "duration_ns",
    "start_time",
    "end_time",
    "attributes",
];

/// Column names for the canonical `logs` virtual table.
///
/// | Column     | Type      | Description                    |
/// |------------|-----------|--------------------------------|
/// | timestamp  | TIMESTAMP | Log event time (UTC)           |
/// | level      | TEXT      | trace, debug, info, warn, error|
/// | service    | TEXT      | Originating service name       |
/// | message    | TEXT      | Log message body               |
/// | attributes | JSONB     | Arbitrary key-value attributes |
pub const LOG_COLUMNS: &[&str] = &["timestamp", "level", "service", "message", "attributes"];

/// Column names for the canonical `metrics` virtual table.
///
/// | Column      | Type      | Description                    |
/// |-------------|-----------|--------------------------------|
/// | metric_name | TEXT      | Metric name / key              |
/// | timestamp   | TIMESTAMP | Observation time (UTC)         |
/// | value       | DOUBLE    | Metric value                   |
/// | tags        | JSONB     | Arbitrary key-value tags       |
pub const METRIC_COLUMNS: &[&str] = &["metric_name", "timestamp", "value", "tags"];

/// Total number of columns in the spans table.
pub const SPAN_COLUMN_COUNT: usize = SPAN_COLUMNS.len();

/// Total number of columns in the logs table.
pub const LOG_COLUMN_COUNT: usize = LOG_COLUMNS.len();

/// Total number of columns in the metrics table.
pub const METRIC_COLUMN_COUNT: usize = METRIC_COLUMNS.len();

/// Check whether a column name belongs to the spans schema.
pub fn is_span_column(name: &str) -> bool {
    SPAN_COLUMNS.contains(&name)
}

/// Check whether a column name belongs to the logs schema.
pub fn is_log_column(name: &str) -> bool {
    LOG_COLUMNS.contains(&name)
}

/// Check whether a column name belongs to the metrics schema.
pub fn is_metric_column(name: &str) -> bool {
    METRIC_COLUMNS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_span_columns_has_required_fields() {
        assert!(is_span_column("trace_id"));
        assert!(is_span_column("span_id"));
        assert!(is_span_column("parent_span_id"));
        assert!(is_span_column("service"));
        assert!(is_span_column("operation"));
        assert!(is_span_column("status"));
        assert!(is_span_column("duration_ns"));
        assert!(is_span_column("start_time"));
        assert!(is_span_column("end_time"));
        assert!(is_span_column("attributes"));
        assert_eq!(SPAN_COLUMN_COUNT, 10);
    }

    #[test]
    fn test_log_columns_has_required_fields() {
        assert!(is_log_column("timestamp"));
        assert!(is_log_column("level"));
        assert!(is_log_column("service"));
        assert!(is_log_column("message"));
        assert!(is_log_column("attributes"));
        assert_eq!(LOG_COLUMN_COUNT, 5);
    }

    #[test]
    fn test_metric_columns_has_required_fields() {
        assert!(is_metric_column("metric_name"));
        assert!(is_metric_column("timestamp"));
        assert!(is_metric_column("value"));
        assert!(is_metric_column("tags"));
        assert_eq!(METRIC_COLUMN_COUNT, 4);
    }

    #[test]
    fn test_unknown_columns_rejected() {
        assert!(!is_span_column("nonexistent"));
        assert!(!is_log_column("nonexistent"));
        assert!(!is_metric_column("nonexistent"));
    }
}
