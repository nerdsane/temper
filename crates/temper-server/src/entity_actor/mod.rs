//! Generic entity actor powered by JIT transition tables.
//!
//! This is the bridge between the actor runtime and the state machine specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.

mod actor;
pub mod effects;
pub mod sim_handler;
pub mod types;

pub use actor::EntityActor;
pub use effects::{
    ProcessResult, ScheduledAction, apply_effects, apply_new_state_fallback, build_eval_context,
    process_action, sync_fields,
};
pub use sim_handler::EntityActorHandler;
pub use types::{EntityEvent, EntityMsg, EntityResponse, EntityState};
