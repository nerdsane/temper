//! Shared effect application for server entity actors.

mod canonical;
mod core;
mod fields;

pub use canonical::{
    MAX_CROSS_ENTITY_LOOKUPS, MAX_SPAWNS_PER_TRANSITION, build_eval_context,
    build_eval_context_with_xref,
};
pub use core::{
    FieldSyncMode, ProcessResult, apply_effects, apply_new_state_fallback, process_action,
    process_action_with_xref, process_action_with_xref_and_field_mode,
    resolve_schedule_at_requests,
};
pub use core::{ScheduleAtRequest, ScheduledAction, SpawnRequest};
pub(crate) use fields::prune_transient_action_fields_from_state;
pub use fields::{DEFAULT_FIELD_INLINE_MAX, sync_fields, sync_fields_with_metadata};

#[cfg(test)]
#[path = "effects/action_test.rs"]
mod action_tests;
#[cfg(test)]
#[path = "effects/command_tests.rs"]
mod command_tests;
#[cfg(test)]
#[path = "effects/field_test.rs"]
mod field_tests;
