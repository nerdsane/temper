//! Successful action telemetry, snapshotting, assertions, and reply.

use super::*;
use crate::entity_actor::effects::ProcessResult;

impl EntityActor {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finalize_successful_action(
        &self,
        duration_ns: u64,
        name: &str,
        state: &mut EntityState,
        event: EntityEvent,
        persisted_timeout_clock: Option<PersistedStateTimeoutClock>,
        committed_table: &TransitionTable,
        event_count_before: usize,
        result: ProcessResult,
        idempotency_key: Option<&str>,
        actor_key: &str,
        ctx: &mut ActorContext<Self>,
    ) {
        // Telemetry as Views: duration covers evaluate + effects + persist.
        let wide = wide_event::from_transition(wide_event::TransitionInput {
            tenant: &self.tenant,
            entity_type: &state.entity_type,
            entity_id: &state.entity_id,
            operation: name,
            from_status: &event.from_status,
            to_status: &state.status,
            success: true,
            duration_ns,
            params: &event.params,
            item_count: state.item_count,
            trace_id: &self.trace_id,
        });
        wide_event::emit_span(&wide);
        wide_event::emit_metrics(&wide);

        if let Some(clock) = persisted_timeout_clock {
            apply_state_timeout_clock(state, clock);
        } else {
            Self::update_state_timeout_clock(committed_table, state, &event);
        }
        state.push_event_bounded(event);

        let persistence_id = self.persistence_id();
        if let Some(ref store) = self.event_journal
            && let Err(e) = Self::maybe_save_snapshot(
                store,
                self.snapshot_queue.as_ref(),
                &persistence_id,
                state,
            )
            .await
        {
            tracing::warn!(
                entity = %state.entity_id,
                seq = state.sequence_nr,
                error = %e,
                "failed to persist snapshot"
            );
        }

        debug_assert!(
            committed_table.states.contains(&state.status),
            "POSTCONDITION: status '{}' not in valid states after {}",
            state.status,
            name
        );
        debug_assert!(
            state.total_event_count == event_count_before + 1,
            "POSTCONDITION: event count must grow by exactly 1 (was {}, now {})",
            event_count_before,
            state.total_event_count
        );
        debug_assert!(
            state
                .events
                .back()
                .expect("events non-empty after push")
                .action
                == name, // ci-ok: post-assertion, just pushed an event
            "POSTCONDITION: last event must be the action that just fired"
        );

        tracing::info!(
            entity = %state.entity_id,
            action = %name,
            to = %state.status,
            events_total = state.total_event_count,
            events_since_snapshot = state.events_since_snapshot,
            events_recent = state.events.len(),
            "transition applied"
        );

        let response = EntityResponse {
            success: true,
            state: state.clone(),
            error: None,
            custom_effects: result.custom_effects,
            scheduled_actions: result.scheduled_actions,
            spawn_requests: result.spawn_requests,
            spec_governed: true,
        };
        // ADR-0048 sub-decision 5: cache the successful response so a racing
        // retry returns it instead of re-executing.
        if let (Some(key), Some(cache)) = (idempotency_key, self.idempotency_cache.as_ref()) {
            cache.put(actor_key, key, response.clone());
        }
        ctx.reply(response);
    }
}
