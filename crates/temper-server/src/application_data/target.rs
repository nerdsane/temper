//! Host-owned routing for tenant-global and immutable scoped data targets.

use temper_wasm_sdk::data::ModuleDataError;

use crate::request_context::AgentContext;

use super::{
    ApplicationDataInvocation, GovernedApplicationDataService, ModuleDataTarget, internal_error,
    short_type,
};

impl ApplicationDataInvocation {
    pub(super) async fn get_target_entity(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<crate::entity_actor::EntityResponse, ModuleDataError> {
        let service = GovernedApplicationDataService::new(&self.state);
        match &self.authority.target {
            ModuleDataTarget::TenantGlobal => {
                service
                    .get(&self.authority.tenant, short_type(entity_type), entity_id)
                    .await
            }
            ModuleDataTarget::Scoped(pin) => {
                service
                    .get_scoped(
                        &self.authority.tenant,
                        short_type(entity_type),
                        entity_id,
                        pin.clone(),
                    )
                    .await
            }
        }
        .map_err(internal_error)
    }

    pub(super) async fn target_entity_exists(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<bool, ModuleDataError> {
        match &self.authority.target {
            ModuleDataTarget::TenantGlobal => Ok(self.state.entity_exists(
                &self.authority.tenant,
                short_type(entity_type),
                entity_id,
            )),
            ModuleDataTarget::Scoped(pin) => GovernedApplicationDataService::new(&self.state)
                .exists_scoped(
                    &self.authority.tenant,
                    short_type(entity_type),
                    entity_id,
                    pin,
                )
                .await
                .map_err(internal_error),
        }
    }

    pub(super) fn operation_agent_context(&self, expected_sequence: Option<u64>) -> AgentContext {
        AgentContext {
            security_ctx: Some(self.authority.security.clone()),
            agent_id: Some(self.authority.security.principal.id.clone()),
            expected_entity_sequence: expected_sequence,
            schema_pin: self.authority.target.schema_pin().cloned(),
            ..AgentContext::default()
        }
    }
}
