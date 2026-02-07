//! temper-server: HTTP server assembly for Temper entity services.
//!
//! Composes OData routing and actor dispatch into an axum server.
//! This is the blocking HTTP boundary — incoming requests are parsed,
//! dispatched to the non-blocking actor core, and responses serialized.

mod router;
mod dispatch;
mod response;
mod state;

pub use router::build_router;
pub use state::ServerState;
