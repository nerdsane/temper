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

//!
//! ## Selecting a schema
//!
//! `PostgresEventStore::new(pool)` retains the connection's existing search-path
//! behavior. To select a schema independently of transaction-pooler defaults:
//!
//! ```no_run
//! # async fn example(pool: sqlx::PgPool) -> Result<(), temper_runtime::persistence::PersistenceError> {
//! use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};
//! let schema = PostgresSchema::new("application_storage")?;
//! migration::run_migrations_in_schema(&pool, &schema).await?;
//! let store = PostgresEventStore::with_schema(pool, schema);
//! # Ok(()) }
//! ```
//!
//! Migration-managed and externally provisioned schemas are both supported; skip
//! the migration call when the operator provisions tables. Runtime operations
//! never create or move tables. Keep the configured store when constructing the
//! server storage stack, and use `restore_registry_from_postgres_store` when
//! restoring the registry. Bare-pool compatibility APIs still use the default
//! search path. Schema selection is independent of Temper tenant authorization.

mod data_only_create;
pub mod dbm;
mod entity_counts;
mod metrics;
mod namespace;
pub use namespace::PostgresSchema;
pub mod migration;
pub mod platform;
mod query_page;
pub mod schema;
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

#[cfg(test)]
mod policy_replacement_test;
