//! temper-server: HTTP server assembly for Temper entity services.
//!
//! Composes OData routing and actor dispatch into an axum server.
//! The entity actor uses JIT TransitionTables for state machine transitions,
//! ensuring the same logic verified by DST runs in production.

mod constraint_engine;
mod dispatch;
pub mod entity_actor;
pub mod event_store;
pub mod events;
#[cfg(feature = "observe")]
pub mod observe_routes;
mod query_eval;
pub mod reaction;
pub mod registry;
mod response;
mod router;
#[cfg(feature = "observe")]
pub mod sentinel;
pub mod state;
pub mod webhooks;

pub use entity_actor::{EntityActor, EntityActorHandler, EntityMsg, EntityResponse, EntityState};
pub use event_store::ServerEventStore;
pub use registry::SpecRegistry;
pub use router::build_router;
pub use state::ServerState;
