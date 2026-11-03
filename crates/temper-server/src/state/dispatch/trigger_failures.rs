//! The answer to an inline action whose triggers did not all succeed (ADR-0184).
//!
//! The action committed before its triggers ran, so its answer says so. A
//! trigger that failed is reported beside it and compensated on the record as
//! a background failure is (ADR-0152); an answer that was refused is reported
//! only, since its refusal is already on the record. A composite action is the
//! exception: its effect is its integration's sub-writes, so when that fails
//! the action failed (ADR-0140).

use super::effects::PostDispatchContext;
use crate::entity_actor::EntityResponse;

impl crate::state::ServerState {
    /// Answer `response`, the committed action, after its inline triggers ran:
    /// `answered` is the last answer an integration sent back, and `failures`
    /// the triggers that failed.
    pub(super) fn answer_failed_triggers(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: EntityResponse,
        answered: Option<EntityResponse>,
        failures: Vec<String>,
    ) -> EntityResponse {
        assert!(
            !failures.is_empty() || answered.as_ref().is_some_and(|answer| !answer.success),
            "answer_failed_triggers: no trigger failed"
        );
        if self.action_is_composite(ctx) {
            return self.fail_composite(ctx, response, answered, failures);
        }
        if !failures.is_empty() {
            self.dispatch_integration_failure_compensation(
                ctx.tenant,
                ctx.entity_type,
                ctx.entity_id,
                ctx.action,
                &failures.join("; "),
                ctx.agent_ctx,
            );
        }
        let mut trigger_failures = failures;
        let state = match answered {
            Some(answer) => {
                if !answer.success {
                    trigger_failures.push(format!(
                        "answer refused: {}",
                        answer.error.as_deref().unwrap_or("no reason given")
                    ));
                }
                answer.state
            }
            None => response.state.clone(),
        };
        self.dispatch_transition_deferred_effects(ctx, &response);
        EntityResponse {
            state,
            trigger_failures,
            store_unavailable: false,
            ..response
        }
    }

    /// Whether the action is composite. A table that cannot be read counts as
    /// composite, so a failed integration is never answered as success.
    fn action_is_composite(&self, ctx: &PostDispatchContext<'_>) -> bool {
        match self.transition_table_for_dispatch(ctx.tenant, ctx.entity_type) {
            Ok(table) => table.composite_actions.contains_key(ctx.action),
            Err(_) => true,
        }
    }

    /// A composite whose integration failed has failed, and its caller gets
    /// the failure. A refused answer comes back as it was refused. The
    /// committed transition's scheduled actions still fire.
    fn fail_composite(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: EntityResponse,
        answered: Option<EntityResponse>,
        failures: Vec<String>,
    ) -> EntityResponse {
        if failures.is_empty() {
            let refused = answered.expect("a refused answer when no trigger failed");
            self.dispatch_transition_deferred_effects(ctx, &response);
            return EntityResponse {
                store_unavailable: false,
                ..refused
            };
        }
        self.arm_scheduled_actions(ctx, &response);
        EntityResponse {
            success: false,
            refusal: None,
            error: Some(failures.join("; ")),
            scheduled_actions: Vec::new(),
            spawn_requests: Vec::new(),
            refused: false,
            answered_action: None,
            trigger_failures: Vec::new(),
            store_unavailable: false,
            ..response
        }
    }
}
