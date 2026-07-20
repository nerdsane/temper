//! Action admission checks that can reply without executing a transition.

use super::*;

impl EntityActor {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn reply_to_short_circuited_action(
        &self,
        table: &TransitionTable,
        actor_key: &str,
        name: &str,
        cross_entity_booleans: &BTreeMap<String, bool>,
        idempotency_key: Option<&str>,
        state: &EntityState,
        state_timeout_precondition: Option<&StateTimeoutPrecondition>,
        ctx: &mut ActorContext<Self>,
    ) -> bool {
        // ADR-0048 sub-decision 5: actor-side idempotency dedup. A
        // dispatch-layer retry can reach this actor after the original caller
        // times out, so return the cached response rather than re-executing.
        if let (Some(key), Some(cache)) = (idempotency_key, self.idempotency_cache.as_ref())
            && let Some(cached) = cache.get(actor_key, key)
        {
            ctx.reply(cached);
            return true;
        }
        if let Some(key) = idempotency_key
            && state.has_processed_idempotency_key(key)
        {
            let custom_effects =
                duplicate_idempotency_custom_effects(table, state, name, cross_entity_booleans);
            let mut response_state = state.clone();
            if !custom_effects.is_empty() {
                prune_transient_action_fields_from_state(&mut response_state);
            }
            ctx.reply(EntityResponse {
                success: true,
                state: response_state,
                error: None,
                custom_effects,
                scheduled_actions: vec![],
                spawn_requests: vec![],
                spec_governed: true,
            });
            return true;
        }

        // State-timeout actions carry the state and durable clock anchor
        // observed when their timer was armed. Validate immediately before
        // transition evaluation so a newer reset makes this timeout a no-op.
        if state_timeout_precondition_is_stale(table, state, state_timeout_precondition) {
            ctx.reply(EntityResponse {
                success: false,
                state: state.clone(),
                error: Some(STATE_TIMEOUT_PRECONDITION_MISMATCH.to_string()),
                custom_effects: vec![],
                scheduled_actions: vec![],
                spawn_requests: vec![],
                spec_governed: true,
            });
            return true;
        }

        // TigerStyle: assert and enforce the per-entity budgets before every
        // transition. The assertions catch corrupt state; the explicit check
        // gives a stable response when the event budget is legitimately spent.
        debug_assert!(
            table.states.contains(&state.status),
            "PRECONDITION: status '{}' not in valid states {:?}",
            state.status,
            table.states
        );
        debug_assert!(
            state.events_since_snapshot < MAX_EVENTS_SINCE_SNAPSHOT,
            "PRECONDITION: event budget exhausted ({} >= {})",
            state.events_since_snapshot,
            MAX_EVENTS_SINCE_SNAPSHOT
        );
        debug_assert!(
            state.item_count <= MAX_ITEMS_PER_ENTITY,
            "PRECONDITION: item budget exceeded ({} > {})",
            state.item_count,
            MAX_ITEMS_PER_ENTITY
        );

        if state.events_since_snapshot < MAX_EVENTS_SINCE_SNAPSHOT {
            return false;
        }

        let workspace_id = event_budget_workspace_id(state);
        crate::event_budget_metrics::record_exhausted(
            &self.tenant,
            &state.entity_type,
            &state.entity_id,
            &workspace_id,
        );
        tracing::warn!(
            tenant = %self.tenant,
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            workspace_id = %workspace_id,
            status = %state.status,
            action = %name,
            events_since_snapshot = state.events_since_snapshot,
            total_event_count = state.total_event_count,
            max_events_since_snapshot = MAX_EVENTS_SINCE_SNAPSHOT,
            "Event budget exhausted (10000 max since snapshot)"
        );
        ctx.reply(EntityResponse {
            success: false,
            state: state.clone(),
            error: Some(format!(
                "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
            )),
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        });
        true
    }
}
