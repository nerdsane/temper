//! Public reaction fanout entry point.

use crate::request_context::AgentContext;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use super::super::super::types::ReactionResult;
use super::super::ReactionDispatcher;

impl ReactionDispatcher {
    /// Dispatch reactions triggered by a successful entity action.
    ///
    /// This is called after the source action has been committed and the SSE
    /// broadcast sent. Reactions are fire-and-forget: failures are logged but
    /// do not roll back the source transition.
    #[expect(
        clippy::too_many_arguments,
        reason = "reaction dispatch binds source authority and transition identity"
    )]
    #[instrument(skip_all, fields(
        otel.name = "reaction.dispatch",
        tenant = %tenant,
        entity_type,
        entity_id,
        action_name = action,
        depth,
        reaction.rule_count = tracing::field::Empty,
        reaction.fired_count = tracing::field::Empty,
        reaction.guard_skipped_count = tracing::field::Empty,
        reaction.target_resolve_error_count = tracing::field::Empty,
        reaction.authz_denied_count = tracing::field::Empty,
        reaction.dispatch_error_count = tracing::field::Empty,
        reaction.success_count = tracing::field::Empty,
        reaction.result_count = tracing::field::Empty,
    ))]
    pub async fn dispatch_reactions(
        &self,
        state: &crate::ServerState,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        to_state: &str,
        fields: &serde_json::Value,
        depth: u32,
        invoking_ctx: &AgentContext,
    ) -> Vec<ReactionResult> {
        let rules: Vec<_> = if let Some(pin) = invoking_ctx.schema_pin.as_ref() {
            state
                .registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .scoped_reaction_candidates_at_digest(
                    tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                    entity_type,
                    action,
                )
                .into_iter()
                .filter(|rule| {
                    rule.when
                        .to_state
                        .as_deref()
                        .is_none_or(|state| state == to_state)
                })
                .collect()
        } else {
            self.registry
                .lookup(tenant, entity_type, action, to_state)
                .into_iter()
                .cloned()
                .collect()
        };

        self.dispatch_rules(
            state,
            tenant,
            entity_type,
            entity_id,
            action,
            to_state,
            fields,
            depth,
            invoking_ctx,
            rules,
            None,
        )
        .await
    }
}
