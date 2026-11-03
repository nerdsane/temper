//! Generic entity actor powered by JIT transition tables.
//!
//! This is the bridge between the actor runtime and the state machine specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.

pub(crate) mod action_input;
mod actor;
pub(crate) mod bootstrap;
mod effect_outcome;
pub mod effects;
pub(crate) mod entity_rows;
mod field_updates;
mod last_event;
pub use last_event::{EventOrigin, EventVia, LastEvent};
pub(crate) use timer_needs_person::Stop as TimerStop;
mod move_invariants;
pub(crate) mod related;
mod replay_validation;
mod scheduled_rows;
pub mod sim_handler;
mod snapshot_queue;
pub(crate) mod spawn_refusal;
mod timer_fire;
pub(crate) mod timer_needs_person;
mod tombstone;
pub mod types;
pub(crate) mod undeclared_params;
mod unrecorded;

pub use actor::EntityActor;
pub(crate) use actor::RehydrateHook;
#[cfg(test)]
pub(crate) use actor::recover_authoritative_entity_state_from_store;
pub(crate) use actor::{recover_entity_state_for_walk, recover_entity_state_from_store};
pub use effects::{
    ProcessResult, ScheduledAction, apply_effects, apply_new_state_fallback, build_eval_context,
    build_eval_context_with_xref, process_action, process_action_with_xref, sync_fields,
};
pub(crate) use move_invariants::broken_invariant;
pub use sim_handler::EntityActorHandler;
pub(crate) use snapshot_queue::SnapshotWriteQueue;
pub use types::{
    EffectOutcome, EntityEvent, EntityMsg, EntityResponse, EntityState, ProcessedIdempotencyKey,
    StatusClock,
};
