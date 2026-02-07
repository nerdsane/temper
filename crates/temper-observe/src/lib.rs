//! temper-observe: Observability and behavioral telemetry for Temper.
//!
//! Captures runtime traces, state transitions, and invariant checks,
//! storing them for analysis and TLA+ conformance verification.
//!
//! # Architecture
//!
//! The crate centres on the [`ObservabilityStore`] trait -- a SQL query
//! interface over canonical virtual tables (`spans`, `logs`, `metrics`).
//! Provider adapters (Logfire, Datadog, etc.) implement this trait, while
//! sentinel actors and Evolution Records consume it.
//!
//! ## Modules
//!
//! - [`error`] -- Error types.
//! - [`store`] -- The `ObservabilityStore` trait and result types.
//! - [`schema`] -- Canonical virtual-table column definitions.
//! - [`memory`] -- In-memory adapter for testing.
//! - [`trajectory`] -- Trajectory context and outcome types.

pub mod error;
pub mod memory;
pub mod schema;
pub mod store;
pub mod trajectory;
pub mod clickhouse;
pub mod wide_event;

// Re-export the most commonly used types at the crate root.
pub use error::ObserveError;
pub use memory::InMemoryStore;
pub use schema::{LOG_COLUMNS, METRIC_COLUMNS, SPAN_COLUMNS};
pub use store::{ObservabilityStore, ResultRow, ResultSet, SqlParam};
pub use trajectory::{TrajectoryContext, TrajectoryOutcome};
pub use clickhouse::{ClickHouseStore, SpanRecord, LogRecord, MetricRecord};
pub use wide_event::{WideEvent, from_transition, project_to_metrics, project_to_span, project_to_log};
