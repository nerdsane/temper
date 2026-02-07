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

pub mod migration;
pub mod schema;
pub mod store;

pub use store::PostgresEventStore;
