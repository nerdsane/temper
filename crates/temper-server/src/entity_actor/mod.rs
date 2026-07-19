//! Generic entity actor powered by JIT transition tables.
//!
//! This is the bridge between the actor runtime and the state machine specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.

mod actor;
pub mod effects;
pub(crate) mod event_persistence;
pub mod sim_handler;
mod snapshot_queue;
pub mod types;

pub use actor::EntityActor;
pub(crate) use actor::recover_entity_state_from_store;
pub use effects::{
    ProcessResult, ScheduledAction, apply_effects, apply_new_state_fallback, build_eval_context,
    process_action, process_action_with_xref, sync_fields,
};
pub use sim_handler::EntityActorHandler;
pub(crate) use snapshot_queue::SnapshotWriteQueue;
pub use types::{EntityEvent, EntityMsg, EntityResponse, EntityState, StateTimeoutPrecondition};
