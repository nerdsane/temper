//! temper-server: HTTP server assembly for Temper entity services.
//!
//! Composes OData routing and actor dispatch into an axum server.
//! The entity actor uses JIT TransitionTables for state machine transitions,
//! ensuring the same logic verified by DST runs in production.

mod router;
mod dispatch;
pub mod events;
mod response;
mod query_eval;
pub mod state;
pub mod entity_actor;
pub mod registry;
pub mod reaction;
#[cfg(feature = "observe")]
pub mod observe_routes;
#[cfg(feature = "observe")]
pub mod sentinel;

pub use router::build_router;
pub use state::ServerState;
pub use registry::SpecRegistry;
pub use entity_actor::{EntityActor, EntityActorHandler, EntityMsg, EntityResponse, EntityState};
