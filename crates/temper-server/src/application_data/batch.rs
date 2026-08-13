//! Bounded non-atomic batch execution and optimistic sequence checks.

use temper_wasm_sdk::data::{
    BatchItemV1, DataOperationKind, DataOperationV1, DataOutcomeV1, DataResultV1, ModuleDataError,
    ModuleDataErrorKind,
};

use super::{ApplicationDataInvocation, data_error, internal_error, short_type};

impl ApplicationDataInvocation {
    pub(super) async fn check_sequence(
        &self,
        entity_type: &str,
        entity_id: &str,
        expected: Option<u64>,
    ) -> Result<(), ModuleDataError> {
        if let Some(expected) = expected {
            let current = self
                .state
                .get_tenant_entity_state(&self.authority.tenant, short_type(entity_type), entity_id)
                .await
                .map_err(internal_error)?;
            if current.state.sequence_nr != expected {
                return Err(data_error(
                    ModuleDataErrorKind::Conflict,
                    "SequenceConflict",
                    "entity sequence does not match expected_sequence",
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn batch(
        &self,
        items: Vec<BatchItemV1>,
    ) -> Result<DataResultV1, ModuleDataError> {
        if !self
            .authority
            .binding
            .grant
            .operations
            .contains(&DataOperationKind::Batch)
        {
            return Err(data_error(
                ModuleDataErrorKind::AuthorizationDenied,
                "CapabilityDenied",
                "module data grant does not permit batch operations",
            ));
        }
        if items.len() > self.authority.binding.grant.budgets.max_batch_items as usize {
            return Err(data_error(
                ModuleDataErrorKind::BudgetExceeded,
                "BatchBudgetExceeded",
                "batch item budget exceeded",
            ));
        }
        let acknowledgement_reservation = items.len().saturating_mul(3_840).saturating_add(256);
        if acknowledgement_reservation
            > self.authority.binding.grant.budgets.max_response_bytes as usize
        {
            return Err(data_error(
                ModuleDataErrorKind::BudgetExceeded,
                "BatchResponseReservationExceeded",
                "batch compact acknowledgement exceeds the response budget",
            ));
        }
        let mut outcomes = Vec::with_capacity(items.len());
        for item in items {
            let operation = match item {
                BatchItemV1::EntityGet {
                    entity_type,
                    entity_id,
                    at_least_sequence,
                } => DataOperationV1::EntityGet {
                    entity_type,
                    entity_id,
                    at_least_sequence,
                },
                BatchItemV1::EntityCreate { entity_type, value } => {
                    DataOperationV1::EntityCreate { entity_type, value }
                }
                BatchItemV1::EntityPatch {
                    entity_type,
                    entity_id,
                    expected_sequence,
                    value,
                } => DataOperationV1::EntityPatch {
                    entity_type,
                    entity_id,
                    expected_sequence,
                    value,
                },
                BatchItemV1::ActionInvoke {
                    entity_type,
                    entity_id,
                    action,
                    expected_sequence,
                    params,
                } => DataOperationV1::ActionInvoke {
                    entity_type,
                    entity_id,
                    action,
                    expected_sequence,
                    params,
                },
            };
            outcomes.push(match Box::pin(self.execute_inner(operation)).await {
                Ok(result) => DataOutcomeV1::Ok { result },
                Err(error) => DataOutcomeV1::Error { error },
            });
        }
        Ok(DataResultV1::Batch { outcomes })
    }
}
