//! Commit visibility required by work launched from a transition. Ordinary
//! transitions retain queued projections; dependent readers cannot use them.
use super::effects::PostDispatchContext;
use crate::entity_actor::EntityResponse;
use crate::state::ServerState;
use std::time::Duration;
use temper_runtime::persistence::PersistenceError;

const PROJECTION_BARRIER_BUDGET: Duration = Duration::from_secs(30);

impl ServerState {
    pub(super) fn has_projection_dependents(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) -> bool {
        if !response.custom_effects.is_empty()
            || !response.spawn_requests.is_empty()
            || !response.scheduled_actions.is_empty()
        {
            return true;
        }
        if self.webhook_dispatcher.as_ref().is_some_and(|dispatcher| {
            dispatcher.configs().iter().any(|config| {
                (config.actions.is_empty() || config.actions.iter().any(|a| a == ctx.action))
                    && (config.entity_types.is_empty()
                        || config.entity_types.iter().any(|t| t == ctx.entity_type))
            })
        }) {
            return true;
        }
        if self
            .registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_spec(ctx.tenant, ctx.entity_type)
            .is_some_and(|spec| {
                spec.automaton
                    .state_timeouts
                    .iter()
                    .any(|timeout| timeout.state == response.state.status)
            })
        {
            return true;
        }
        self.reaction_dispatcher
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|dispatcher| {
                dispatcher.has_reactions(
                    ctx.tenant,
                    ctx.entity_type,
                    ctx.action,
                    &response.state.status,
                )
            })
    }

    /// Await the actual sequence-guarded store write, not queue admission.
    /// Failure leaves the journal committed and must prevent dependent dispatch.
    pub(super) async fn project_before_dependents(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) -> Result<(), PersistenceError> {
        let Some(query_plane) = self.query_plane_store() else {
            return Ok(());
        };
        let fields =
            self.query_projection_fields(ctx.tenant, ctx.entity_type, &response.state.fields);
        let state = self.query_projection_state(&response.state);
        // Heap allocate the store future: recursive reaction dispatch must not
        // grow every dispatch frame with another storage future.
        // Preserve the sequence guard even for Deleted. Ordered queue cleanup
        // removes that row behind older queued writes.
        tokio::time::timeout(
            PROJECTION_BARRIER_BUDGET,
            Box::pin(query_plane.upsert_projection(
                ctx.tenant.as_str(),
                ctx.entity_type,
                ctx.entity_id,
                &response.state.status,
                &fields,
                &state,
                response.state.sequence_nr,
            )),
        )
        .await
        .map_err(|_| {
            PersistenceError::Storage(format!(
                "query projection timed out after {} seconds",
                PROJECTION_BARRIER_BUDGET.as_secs()
            ))
        })?
    }
}
