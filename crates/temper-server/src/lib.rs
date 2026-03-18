//! temper-server: HTTP server assembly for Temper entity services.
//!
//! Composes OData routing and actor dispatch into an axum server.
//! The entity actor uses JIT TransitionTables for state machine transitions,
//! ensuring the same logic verified by DST runs in production.

pub mod adapters;
#[cfg(feature = "observe")]
mod api;
pub mod authz;
pub mod entity_actor;
pub mod event_store;
pub mod events;
pub mod eventual_invariants;
pub mod idempotency;
pub mod identity;
#[cfg(feature = "observe")]
pub mod observe;
pub mod odata;
pub mod platform_store;
mod query_eval;
pub mod reaction;
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
pub mod wasm_registry;
pub mod webhooks;

pub use entity_actor::{EntityActor, EntityActorHandler, EntityMsg, EntityResponse, EntityState};
pub use event_store::ServerEventStore;
pub use registry::SpecRegistry;
pub use router::build_router;
pub use state::ServerState;
