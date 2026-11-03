//! Mix crate — control plane + default runtime + apply. Read last.
//!
//! HTTP / OData / Cedar / registry live here with EntityActor and
//! `entity_actor/effects.rs` (the default `apply_effects`). Not a plug.

// The wasm host's async blocks exceed rustc's default query depth.
#![recursion_limit = "256"]

pub mod adapters;
mod admin;
#[cfg(feature = "observe")]
mod api;
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
mod growable_stack;
#[cfg(feature = "observe")]
pub mod host_room;
#[cfg(feature = "observe")]
pub mod host_turn;
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
pub mod secrets;
#[cfg(feature = "observe")]
pub mod sentinel;
pub mod state;
mod static_web;
pub mod storage;
#[cfg(test)]
pub(crate) mod test_spans;
pub(crate) mod trace_context;
pub(crate) mod trajectory_outbox;
pub mod trigger;
pub mod vector_index;
pub mod wake;
pub mod wasm_registry;
pub mod webhooks;
pub mod write_health;

// Reads RSS from procfs, which only Linux has.
#[cfg(all(test, target_os = "linux"))]
mod rss_pin_suspects;

pub use entity_actor::{EntityActor, EntityActorHandler, EntityMsg, EntityResponse, EntityState};
pub use registry::SpecRegistry;
pub use router::{build_router, build_routes, http_endpoint_fallback, with_request_span};
pub use state::ServerState;
pub use storage::StorageStack;
