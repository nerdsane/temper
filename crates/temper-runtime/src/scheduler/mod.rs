//! Deterministic simulation scheduler and actor system.
//!
//! Provides a single-threaded, seed-controlled message delivery system
//! inspired by FoundationDB's simulation testing and TigerBeetle's VOPR.

pub mod clock;
pub mod context;
mod core;
pub mod id_gen;
pub mod rng;
pub mod sim_actor_system;
pub mod sim_handler;
pub mod types;

// Re-export key types from submodules.
pub use self::core::SimScheduler;
pub use clock::{LogicalClock, SimClock, WallClock};
pub use context::{
    SimContextGuard, install_deterministic_context, install_sim_context, sim_now, sim_uuid,
};
pub use id_gen::{DeterministicIdGen, RealIdGen, SimIdGen};
pub use rng::DeterministicRng;
pub use sim_actor_system::{
    ActorInvariantViolation, RunRecord, SimActorResult, SimActorSystem, SimActorSystemConfig,
    SimIntegrationResponses,
};
pub use sim_handler::{CompareOp, SimActorHandler, SpecAssert, SpecInvariant};
pub use types::{FaultConfig, SimActorState, SimMessage, SimTime};
