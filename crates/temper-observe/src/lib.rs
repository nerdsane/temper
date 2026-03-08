//! Observability and behavioral telemetry for Temper.
//!
//! Centres on the [`ObservabilityStore`] trait — a SQL query interface over
//! canonical virtual tables (`spans`, `logs`, `metrics`). Provider adapters
//! implement this trait; sentinel actors and Evolution Records consume it.

pub mod clickhouse;
pub mod error;
pub mod memory;
pub mod otel;
pub mod schema;
pub mod store;
pub mod trajectory;
pub mod wide_event;

// Re-export the most commonly used types at the crate root.
pub use clickhouse::ClickHouseStore;
pub use error::ObserveError;
pub use memory::InMemoryStore;
pub use schema::{LOG_COLUMNS, METRIC_COLUMNS, SPAN_COLUMNS};
pub use store::{ObservabilityStore, ResultRow, ResultSet, SqlParam};
pub use trajectory::{TrajectoryContext, TrajectoryOutcome};
pub use wide_event::{
    AuthzDecisionInput, InvariantCheckInput, TransitionInput, WasmInvocationInput, WideEvent,
    emit_metrics, emit_span, from_transition,
};
