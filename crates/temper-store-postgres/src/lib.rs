//! # temper-store-postgres
//!
//! PostgreSQL storage backend for the Temper actor framework.
//!
//! This crate implements the [`EventStore`](temper_runtime::persistence::EventStore)
//! trait from `temper-runtime` using PostgreSQL (via `sqlx`). It provides:
//!
//! - **Event journal** — append-only, JSONB-encoded domain events with
//!   optimistic concurrency control enforced by a unique constraint.
//! - **Snapshot store** — binary snapshots keyed by entity, with upsert
//!   semantics so only the latest snapshot is retained.
//! - **Schema migration** — a simple, idempotent migration runner that
//!   creates the required tables on startup.

mod data_only_create;
pub mod dbm;
mod metrics;
pub mod migration;
pub mod platform;
mod query_page;
pub mod schema;
mod schema_deployment;
mod schema_event_history;
mod segments;
mod selected_catalog;
pub mod store;

pub use metrics::init_metrics;
pub use platform::{
    PostgresActionStats, PostgresAgentSummary, PostgresDesignTimeEventRow,
    PostgresEvolutionRecordInsert, PostgresEvolutionRecordRow, PostgresFeatureRequestRow,
    PostgresInstalledAppRow, PostgresOtsTrajectoryDocument, PostgresOtsTrajectoryParams,
    PostgresOtsTrajectoryRow, PostgresPolicyApprovalCommit, PostgresPolicyDenialPatternRow,
    PostgresPolicyRow, PostgresProjectedEntityFieldsRow, PostgresPublishedArtifactRow,
    PostgresPublishedArtifactUpsert, PostgresQueuedOtsTrajectoryRow, PostgresSecretRow,
    PostgresSpecRow, PostgresSpecVerificationUpdate, PostgresTrajectoryInsert,
    PostgresTrajectoryRow, PostgresTrajectoryStats, PostgresUnmetIntentAggRow,
    PostgresWasmInvocationInsert, PostgresWasmInvocationRow, PostgresWasmModuleMetadataRow,
    PostgresWasmModuleRow,
};
pub use store::PostgresEventStore;
