//! State timers of the transitions a composite batch commits (ADR-0049).
//!
//! A sub-write is a transition like any other. Applied one at a time, each
//! goes through `run_post_dispatch_effects`, which cancels the timers the
//! transition supersedes and arms the ones its state declares. The atomic
//! batch appends every sub-write's event in one journal write instead, so it
//! commits the last timer mutation for each stream with its events. Once
//! committed, it cancels superseded live timers and arms the committed ones.
//! Each timer fires as the sub-write's agent, the caller whose action set
//! the composite going.

use super::*;
use crate::entity_actor::EntityResponse;
use crate::state::ServerState;
use crate::state::dispatch::effects::PostDispatchContext;

/// One staged sub-write on an entity type that declares state timers, with
/// the entity's state just after it.
pub(super) struct TimedTransition<'a> {
    write: &'a PreparedCompositeSubWrite,
    agent_ctx: AgentContext,
    response: EntityResponse,
}

impl TimedTransition<'_> {
    /// The same durable timer mutation a single dispatch commits. Capture it
    /// at this transition, before later sub-writes can change the fields.
    pub(super) fn timer_row(
        &self,
        server: &ServerState,
        tenant: &TenantId,
    ) -> Option<temper_runtime::persistence::AppendStateTimer> {
        server.committed_timer_row(
            tenant,
            &self.write.entity_type,
            &crate::entity_actor::entity_rows::RowsInput {
                state: &self.response.state,
                event: self
                    .response
                    .state
                    .events
                    .back()
                    .expect("staged transition has an event"),
                caller: Some(&self.agent_ctx),
            },
        )
    }

    /// The context the sub-write's own dispatch would hand its post-dispatch
    /// effects.
    fn post_dispatch_context<'c>(&'c self, tenant: &'c TenantId) -> PostDispatchContext<'c> {
        PostDispatchContext {
            tenant,
            entity_type: &self.write.entity_type,
            entity_id: &self.write.entity_id,
            action: &self.write.action,
            agent_ctx: &self.agent_ctx,
            dispatch_idempotency_key: Some(&self.write.idempotency_key),
            action_params: &self.write.params,
            await_integration: false,
        }
    }
}

/// The agent a composite sub-write runs as: the composite's caller, under the
/// sub-write's own idempotency key. Both the atomic batch and the per-write
/// fallback use it, so a sub-write's timers fire as the same identity on
/// either path. Whether the caller could read a record is about that record
/// alone (`timer_standing`), so the composite's answer is dropped: the
/// per-write fallback asks again for its own record, and the atomic batch's
/// timers fire as before.
pub(super) fn composite_sub_write_agent(
    composite_agent_ctx: &AgentContext,
    write: &PreparedCompositeSubWrite,
) -> AgentContext {
    let mut agent = composite_agent_ctx.clone();
    agent.idempotency_key = Some(crate::idempotency::IdempotencyKey::Request(
        write.idempotency_key.clone(),
    ));
    agent.read_when_set = None;
    agent.read_when_set_for = None;
    agent
}

impl crate::state::ServerState {
    /// Keep a staged sub-write for timer bookkeeping when its entity type
    /// declares state timers, with `state` as it stands right after the
    /// sub-write. Other entity types have no timer to arm or cancel.
    pub(super) fn timed_transition<'a>(
        &self,
        parent: &AtomicCompositeParent<'_>,
        write: &'a PreparedCompositeSubWrite,
        state: &EntityState,
    ) -> Option<TimedTransition<'a>> {
        debug_assert_eq!(state.events.back().map(|e| &e.action), Some(&write.action));
        let declares_timers = self.registry.read().ok().is_some_and(|registry| {
            registry
                .get_spec(parent.tenant, &write.entity_type)
                .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty())
        });
        if !declares_timers {
            return None;
        }
        Some(TimedTransition {
            write,
            // Rooted as `dispatch_tenant_action_core` roots the fallback's
            // dispatch of the same sub-write.
            agent_ctx: composite_sub_write_agent(parent.agent_ctx, write)
                .for_dispatch_root(&write.entity_type, &write.entity_id),
            response: EntityResponse {
                success: true,
                refusal: None,
                state: state.clone(),
                error: None,
                custom_effects: Vec::new(),
                scheduled_actions: Vec::new(),
                spawn_requests: Vec::new(),
                spec_governed: true,
                refused: false,
                answered_action: None,
                trigger_failures: Vec::new(),
                store_unavailable: false,
            },
        })
    }

    /// Cancel the timers the committed batch supersedes, in batch order,
    /// before the batch waits for visibility.
    pub(super) fn invalidate_batch_state_timeouts(
        &self,
        tenant: &TenantId,
        timed: &[TimedTransition<'_>],
    ) {
        for transition in timed {
            let ctx = transition.post_dispatch_context(tenant);
            self.invalidate_state_timeouts_if_needed(&ctx, &transition.response);
        }
    }

    /// Arm the timers the committed batch starts, in batch order, once its
    /// writes are visible. An arm from an earlier sub-write that a later one
    /// in the batch superseded is refused by the tracker, as it is for
    /// separate dispatches.
    pub(super) async fn arm_batch_state_timeouts(
        &self,
        tenant: &TenantId,
        timed: &[TimedTransition<'_>],
    ) {
        for transition in timed {
            let ctx = transition.post_dispatch_context(tenant);
            // Boxed as in `run_post_dispatch_effects`, so the timer row's
            // write does not grow the composite's future.
            Box::pin(self.arm_state_timeouts_if_needed(&ctx, &transition.response)).await;
        }
    }
}
