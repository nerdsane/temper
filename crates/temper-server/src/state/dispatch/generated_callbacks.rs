//! Prepare runtime-generated inputs without exempting ordinary action requests.
use super::{WasmDispatchMode, WasmEntityRef};
use crate::{entity_actor::EntityResponse, state::ServerState};
use serde_json::Value;
use temper_runtime::tenant::TenantId;

impl ServerState {
    /// Project generated data onto a strict action's declared inputs. Constraint
    /// checks remain at the actor boundary, against its unmodified live state.
    pub(super) fn prepare_generated_action_params(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        mut params: Value,
    ) -> Result<Value, String> {
        let table = self
            .transition_table_for_dispatch(tenant, entity_type)
            .map_err(|error| error.to_string())?;
        if !table.strict_action_params {
            return Ok(params);
        }
        let contract = table
            .action_contracts
            .get(action)
            .ok_or_else(|| format!("Action '{action}' has no declared parameter contract"))?;
        let fields = params
            .as_object_mut()
            .ok_or_else(|| "Generated action parameters must be a JSON object".to_string())?;
        fields.retain(|name, _| contract.params.contains(name));
        Ok(params)
    }

    pub(super) fn record_generated_callback_refusal(
        &self,
        entity: WasmEntityRef<'_>,
        action: &str,
        error: &str,
    ) {
        let seq = self.next_entity_event_sequence(
            entity.tenant.as_str(),
            entity.entity_type,
            entity.entity_id,
        );
        self.record_entity_observe_event_with_seq(
            entity.tenant.as_str(),
            entity.entity_type,
            entity.entity_id,
            seq,
            "integration_callback_rejected",
            serde_json::json!({
                "action":action, "error":error,
            }),
        );
        tracing::warn!(tenant=%entity.tenant, entity_type=entity.entity_type,
            entity_id=entity.entity_id, action, error, "integration callback rejected");
    }

    /// A rejected callback is an observed refusal, not a new execution failure.
    /// Returning an integration error here would compensate potentially newer state.
    pub(super) async fn reject_generated_callback(
        &self,
        entity: WasmEntityRef<'_>,
        action: &str,
        error: String,
        mode: WasmDispatchMode,
    ) -> Result<Option<EntityResponse>, String> {
        self.record_generated_callback_refusal(entity, action, &error);
        match mode {
            WasmDispatchMode::Background => Ok(None),
            WasmDispatchMode::Inline => {
                let mut response = self
                    .get_tenant_entity_state(entity.tenant, entity.entity_type, entity.entity_id)
                    .await?;
                response.success = false;
                response.error = Some(error);
                Ok(Some(response))
            }
        }
    }
}
