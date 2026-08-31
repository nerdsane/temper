//! Agent Runtime API — thin public wrapper over Temper's IOA actions.
//!
//! Exposes a clean REST surface for agent-run lifecycle:
//!   POST   /v1/agent-runs           → create + configure + provision
//!   GET    /v1/agent-runs/:id        → get run status
//!   POST   /v1/agent-runs/:id/steer → inject a steering message
//!   POST   /v1/agent-runs/:id/cancel → cancel the run
//!
//! Internally dispatches via `ServerState::dispatch_tenant_action` against
//! the existing `TemperAgent` IOA entity — no self-referential HTTP calls.

mod handlers;
mod models;

pub use handlers::build_agent_runtime_router;
