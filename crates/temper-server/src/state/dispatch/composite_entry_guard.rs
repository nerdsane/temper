use temper_runtime::tenant::TenantId;

use crate::entity_actor::effects::build_eval_context_with_xref;
use crate::state::ServerState;

use super::DispatchError;

impl ServerState {
    #[allow(dead_code)]
    pub(super) async fn ensure_composite_entry_transition_allowed(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> Result<(), DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        let current = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
            .map_err(DispatchError::Internal)?;
        let cross_entity_booleans = self
            .resolve_cross_entity_guards(tenant, entity_type, entity_id, action)
            .await;
        let eval_ctx = build_eval_context_with_xref(&current.state, &cross_entity_booleans);

        match table.evaluate_ctx(&current.state.status, &eval_ctx, action) {
            Some(result) if result.success => Ok(()),
            Some(_) => Err(DispatchError::Internal(format!(
                "Composite action '{action}' not valid from state '{}'",
                current.state.status
            ))),
            None => Err(DispatchError::Internal(format!(
                "Unknown composite action: {action}"
            ))),
        }
    }
}
