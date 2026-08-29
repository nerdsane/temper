use super::*;

impl GovernedSchemaDeploymentService<'_> {
    pub(super) async fn authorize_bootstrap(
        &self,
        operation: &SchemaBootstrapOperation,
        security: &SecurityContext,
        invocation: &BootstrapInvocationIdentity,
    ) -> Result<(), ServiceError> {
        let attributes = BTreeMap::from([
            ("tenant".into(), operation.command.tenant.clone().into()),
            (
                "caller_authority".into(),
                operation.command.caller_authority.clone().into(),
            ),
            ("scope_kind".into(), "task".into()),
            ("scope_id".into(), operation.pin.scope.id.clone().into()),
            (
                "bundle_digest".into(),
                operation.pin.bundle_digest.clone().into(),
            ),
            (
                "entity_type".into(),
                operation.command.entity_type.clone().into(),
            ),
            (
                "entity_id".into(),
                operation.command.entity_id.clone().into(),
            ),
            ("module_name".into(), invocation.module_name.clone().into()),
            (
                "module_artifact".into(),
                invocation.artifact_digest.clone().into(),
            ),
            (
                "module_grant_digest".into(),
                invocation.grant_digest.clone().into(),
            ),
            ("module_trigger".into(), invocation.trigger.clone().into()),
            (
                "initial_action".into(),
                operation
                    .command
                    .initial_action
                    .as_ref()
                    .map_or("", |action| action.action.as_str())
                    .to_string()
                    .into(),
            ),
        ]);
        if let Err(denial) = self.state.authorize_with_context(
            security,
            BOOTSTRAP_ACTION,
            "SchemaBootstrap",
            &attributes,
            &operation.command.tenant,
        ) {
            let pending = record_authz_denial(
                self.state,
                DenialInput {
                    tenant: &operation.command.tenant,
                    security_ctx: security,
                    agent_id_override: None,
                    action: BOOTSTRAP_ACTION,
                    resource_type: "SchemaBootstrap",
                    resource_id: &operation.command.entity_id,
                    resource_attrs: serde_json::Value::Object(attributes.into_iter().collect()),
                    reason: &denial.to_string(),
                    module_name: Some(invocation.module_name.clone()),
                    from_status: None,
                    intent: None,
                    session_id: None,
                    spec_governed: None,
                },
            )
            .await;
            return Err(ServiceError::authorization(pending.id));
        }
        Ok(())
    }
}
