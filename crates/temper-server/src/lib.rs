//! temper-server: HTTP server assembly for Temper entity services.
//!
//! Composes OData routing and actor dispatch into an axum server.
//! The entity actor uses JIT TransitionTables for state machine transitions,
//! ensuring the same logic verified by DST runs in production.

pub mod adapters;
mod admin;
#[cfg(feature = "observe")]
mod api;
pub mod application_data;
pub mod authz;
mod aws_sigv4;
pub mod blob_store;
pub mod blob_sweeper;
mod blob_transport_observability;
pub mod blobs;
/// Deterministic conformance checking of a recorded run against its IOA spec.
pub mod conformance;
pub mod entity_actor;
pub mod event_budget_metrics;
pub mod events;
pub mod eventual_invariants;
pub mod http_endpoint;
pub mod idempotency;
pub mod identity;
pub mod internal_invocation;
/// ADR-0153: declared composite-key index hashing (the negative-existence access path).
pub mod key_index;
#[cfg(feature = "observe")]
pub mod observe;
pub mod odata;
pub(crate) mod ots_trajectory_outbox;
pub mod platform_store;
pub mod profiling;
mod query_eval;
mod query_projection_metrics;
pub mod registry;
pub mod registry_bootstrap;
pub mod request_context;
mod response;
mod router;
pub mod runtime_metrics;
mod schema_deployment;
pub mod secrets;
#[cfg(feature = "observe")]
pub mod sentinel;
pub mod state;
mod static_web;
pub mod storage;
pub(crate) mod trajectory_outbox;
pub mod trigger;
pub mod vector_index;
pub mod wasm_registry;
pub mod webhooks;
pub(crate) mod workflow_tracing;

pub use entity_actor::{EntityActor, EntityActorHandler, EntityMsg, EntityResponse, EntityState};
pub use registry::SpecRegistry;
pub use router::build_router;
pub use state::ServerState;
pub use storage::StorageStack;
