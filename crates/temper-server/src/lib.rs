//! temper-server: HTTP server assembly for Temper entity services.
//!
//! Composes OData routing and actor dispatch into an axum server.
//! The entity actor uses JIT TransitionTables for state machine transitions,
//! ensuring the same logic verified by DST runs in production.

mod router;
mod dispatch;
mod response;
mod state;
pub mod entity_actor;

pub use router::build_router;
pub use state::ServerState;
pub use entity_actor::{EntityActor, EntityMsg, EntityResponse, EntityState};
