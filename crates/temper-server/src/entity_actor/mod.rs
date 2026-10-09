//! Generic entity actor powered by JIT transition tables.
//!
//! This is the bridge between the actor runtime and the state machine specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.

pub(crate) mod action_input;
mod actor;
pub(crate) mod admission;
pub(crate) mod bootstrap;
pub mod effects;
pub(crate) mod field_ownership;
mod field_updates;
mod replay_validation;
pub mod sim_handler;
mod snapshot_queue;
pub mod types;

pub use actor::EntityActor;
pub(crate) use actor::{
    recover_authoritative_entity_state_from_store, recover_entity_state_from_store,
};
pub use effects::{
    ProcessResult, ScheduledAction, apply_effects, apply_new_state_fallback, build_eval_context,
    process_action, process_action_with_xref, sync_fields,
};
pub use sim_handler::EntityActorHandler;
pub(crate) use snapshot_queue::SnapshotWriteQueue;
pub use types::{EntityEvent, EntityMsg, EntityResponse, EntityState};

mod idempotency_replay {
    //! Durable verification of reused idempotency keys (ADR-0182).
    //!
    //! When the shared idempotency cache is cold (restart, eviction) or a racing
    //! writer's event was caught up during an optimistic-concurrency retry, a key
    //! found in `processed_idempotency_keys` is verified against the journal event
    //! that carries it, and the original response is rebuilt and checked against
    //! the event's immutable provenance. Keys that cannot be verified fail closed.

    use serde_json::Value;
    use temper_jit::table::TransitionTable;

    use super::actor::{EntityActor, ReplayPolicy, ReplayTarget};
    use super::types::{EntityEvent, EntityResponse, EntityState};
    use crate::idempotency::{
        IDEMPOTENCY_KEY_MISMATCH, IDEMPOTENCY_KEY_UNVERIFIABLE, IDEMPOTENCY_REPLY_UNVERIFIABLE,
        result_digest, unqualified_action,
    };

    /// Outcome of resolving a key that already produced a durable event.
    pub(super) enum ProcessedKeyResolution {
        /// Same logical request: the entity state right after the original event.
        Original(Box<EntityState>),
        /// The key was used for a different action or body.
        Mismatch,
        /// The key is recorded but its original request cannot be verified.
        Unverifiable,
        /// The request matches; the logical reply or completion is unproven.
        ReplyUnverifiable,
    }

    /// Does `event` (which carries the request's key) record this same request?
    ///
    /// Bound events compare the stored binding. Events without a binding are
    /// verified by repeating the transform the actor applied when it journaled
    /// them: `sanitize(normalize_ref(action, params))` must equal the stored
    /// `params`, and the unqualified action name must match.
    fn event_matches_request(
        event: &EntityEvent,
        state: &EntityState,
        action: &str,
        params: &Value,
        binding: &str,
    ) -> bool {
        if let Some(stored) = event.idempotency_binding.as_deref() {
            return stored == binding;
        }
        let action = unqualified_action(action);
        let normalized = super::effects::normalize_ref_action_params(state, action, params);
        let journaled = super::effects::sanitize_action_params(normalized.as_ref());
        event.action == action && event.params == *journaled
    }

    /// Classify a duplicate: a different request is a mismatch; the same
    /// request is answered with `candidate` only if `candidate` still hashes to
    /// the provenance recorded when the event was committed (ADR-0182, review
    /// correction 2). Missing provenance (pre-ADR events) fails closed.
    fn verdict(
        event: &EntityEvent,
        candidate: EntityState,
        request_state: &EntityState,
        action: &str,
        params: &Value,
        binding: &str,
    ) -> ProcessedKeyResolution {
        if !event_matches_request(event, request_state, action, params, binding) {
            return ProcessedKeyResolution::Mismatch;
        }
        match event.idempotency_result.as_deref() {
            Some(recorded)
                if recorded == result_digest(&candidate)
                    && crate::idempotency::verifies_core_reply(event, binding, recorded) =>
            {
                ProcessedKeyResolution::Original(Box::new(candidate))
            }
            _ => ProcessedKeyResolution::ReplyUnverifiable,
        }
    }

    /// A request whose idempotency key already produced a durable event.
    pub(super) struct DuplicateRequest<'a> {
        pub(super) key: &'a str,
        pub(super) action: &'a str,
        pub(super) params: &'a Value,
        pub(super) binding: &'a str,
    }

    /// Failed reply for a mismatched or unverifiable key. Appends nothing.
    pub(super) fn idempotency_rejection(state: &EntityState, error: &str) -> EntityResponse {
        EntityResponse {
            success: false,
            state: state.clone(),
            error: Some(error.to_string()),
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        }
    }

    impl EntityActor {
        /// Resolve a request whose key is already in `processed_idempotency_keys`.
        pub(super) async fn resolve_processed_idempotency_key(
            &self,
            table: &TransitionTable,
            state: &EntityState,
            key: &str,
            action: &str,
            params: &Value,
            binding: &str,
        ) -> ProcessedKeyResolution {
            // The latest event carries the key: the current state is the
            // candidate original response, no replay needed.
            if let Some(last) = state.events.back()
                && last.idempotency_key.as_deref() == Some(key)
            {
                return verdict(last, state.clone(), state, action, params, binding);
            }

            let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend)
            else {
                return ProcessedKeyResolution::Unverifiable;
            };
            let mut replayed = EntityActor::build_initial_state(
                &state.entity_type,
                &state.entity_id,
                table,
                &self.initial_fields,
            );
            let target = ReplayTarget {
                policy: ReplayPolicy::StrictFullJournal,
                stop_after_idempotency_key: Some(key),
            };
            if let Err(error) = Self::replay_events_until(
                table,
                store,
                backend,
                &mut replayed,
                &self.tenant,
                self.blob_store.as_ref(),
                target,
            )
            .await
            {
                tracing::warn!(
                    tenant = %self.tenant,
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    %error,
                    "idempotency key cannot be verified: journal replay failed"
                );
                return ProcessedKeyResolution::Unverifiable;
            }

            match replayed.events.back().cloned() {
                Some(event) if event.idempotency_key.as_deref() == Some(key) => {
                    verdict(&event, replayed, state, action, params, binding)
                }
                _ => ProcessedKeyResolution::Unverifiable,
            }
        }

        /// Reply for a request whose key already produced a durable event.
        ///
        /// The original response is cached as a historical replay so dispatch
        /// never treats it as a newly committed transition (ADR-0182, review
        /// correction 1). Mismatched or unverifiable keys append nothing.
        pub(super) async fn idempotent_duplicate_reply(
            &self,
            table: &TransitionTable,
            state: &EntityState,
            request: DuplicateRequest<'_>,
        ) -> EntityResponse {
            let DuplicateRequest {
                key,
                action,
                params,
                binding,
            } = request;
            let original = match self
                .resolve_processed_idempotency_key(table, state, key, action, params, binding)
                .await
            {
                ProcessedKeyResolution::Original(original) => original,
                ProcessedKeyResolution::Mismatch => {
                    return idempotency_rejection(state, IDEMPOTENCY_KEY_MISMATCH);
                }
                ProcessedKeyResolution::Unverifiable => {
                    return idempotency_rejection(state, IDEMPOTENCY_KEY_UNVERIFIABLE);
                }
                ProcessedKeyResolution::ReplyUnverifiable => {
                    return idempotency_rejection(state, IDEMPOTENCY_REPLY_UNVERIFIABLE);
                }
            };
            // A proven historical core reply carries no new transition work.
            let response_state = *original;
            let response = EntityResponse {
                success: true,
                state: response_state,
                error: None,
                custom_effects: vec![],
                scheduled_actions: vec![],
                spawn_requests: vec![],
                spec_governed: true,
            };
            if let Some(cache) = self.idempotency_cache.as_ref() {
                cache.put_historical(&self.persistence_id(), key, binding, response.clone());
            }
            response
        }
    }
}
