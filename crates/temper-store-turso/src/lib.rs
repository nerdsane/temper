//! # temper-store-turso
//!
//! Turso/libSQL storage backend for the Temper actor framework.
//!
//! This crate implements the [`EventStore`](temper_runtime::persistence::EventStore)
//! trait from `temper-runtime` using libSQL (Turso-compatible).

pub mod schema;
pub mod store;

pub use store::TursoEventStore;
