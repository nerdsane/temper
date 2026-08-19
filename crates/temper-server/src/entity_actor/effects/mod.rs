//! Entity-actor transition application.
//!
//! Mutation of counters, lists, and status is [`temper_jit::apply::apply_effects`]
//! (ADR-0166). This module wraps that function for [`EntityState`], then the
//! actor and dispatch pipeline run the returned schedule/spawn/custom work.

mod apply;
mod fields;
mod process;

#[cfg(test)]
mod tests;

pub use apply::{apply_effects, apply_new_state_fallback, resolve_schedule_at_requests};
pub use fields::{DEFAULT_FIELD_INLINE_MAX, FieldSyncMode, sync_fields, sync_fields_with_metadata};
pub(crate) use fields::{canonicalize_entity_fields, prune_transient_action_fields_from_state};
pub use process::{
    MAX_CROSS_ENTITY_LOOKUPS, MAX_SPAWNS_PER_TRANSITION, ProcessResult, build_eval_context,
    build_eval_context_with_xref, process_action, process_action_with_xref,
    process_action_with_xref_and_field_mode,
};
pub(crate) use process::{entity_authorization_precondition, sanitize_action_params};
pub use temper_jit::apply::{ScheduleAtRequest, ScheduledAction, SpawnRequest};
