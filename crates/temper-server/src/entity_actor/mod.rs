//! Generic entity actor powered by JIT transition tables.
//!
//! This is the bridge between the actor runtime and the state machine specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.

mod actor;
pub mod effects;
pub mod sim_handler;
mod snapshot_queue;
pub mod types;

pub use actor::EntityActor;
pub(crate) use actor::{declared_index_rows, recover_entity_state_from_store};
pub use effects::{
    ProcessResult, ScheduledAction, apply_effects, apply_new_state_fallback, build_eval_context,
    process_action, process_action_with_xref, sync_fields,
};
pub use sim_handler::EntityActorHandler;
pub(crate) use snapshot_queue::SnapshotWriteQueue;
pub use types::{EntityEvent, EntityMsg, EntityResponse, EntityState};

pub(crate) const FIELD_UPDATE_EVENT_TYPE: &str = "$temper.entity.fields-updated.v1";
pub(crate) const SPEC_GENERATION_CHANGED_ERROR: &str =
    "spec generation changed while preparing durable write";

pub(crate) fn is_spec_generation_changed_error(error: &temper_runtime::actor::ActorError) -> bool {
    matches!(
        error,
        temper_runtime::actor::ActorError::Custom(message)
            if message.to_string() == SPEC_GENERATION_CHANGED_ERROR
    )
}

pub(crate) fn is_spec_generation_changed_response(response: &EntityResponse) -> bool {
    !response.success && response.error.as_deref() == Some(SPEC_GENERATION_CHANGED_ERROR)
}

fn validate_domain_action_name(name: &str) -> Result<(), String> {
    if name == FIELD_UPDATE_EVENT_TYPE {
        return Err(format!("action name '{name}' is reserved by Temper"));
    }
    Ok(())
}
