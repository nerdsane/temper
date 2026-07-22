use temper_server::ServerState;

/// Generate Cedar policy from entity state fields.
///
/// Reads agent_id, action_name, resource_type, resource_id, scope, tenant,
/// and scope_matrix from the GovernanceDecision entity's merged fields.
pub(super) async fn handle_generate_cedar_from_fields(
    entity_id: &str,
    fields: &serde_json::Value,
    server: &ServerState,
) -> Result<(), String> {
    if fields
        .get("policy_already_published")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        tracing::debug!(
            entity_id,
            "GenerateCedarPolicy hook skipped because the API already published this policy"
        );
        return Ok(());
    }
    let agent_id = fields
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let action_name = fields
        .get("action_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resource_type = fields
        .get("resource_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resource_id = fields
        .get("resource_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let scope = fields
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("narrow");
    let tenant = fields.get("tenant").and_then(|v| v.as_str()).unwrap_or("");

    if agent_id.is_empty() || action_name.is_empty() || resource_type.is_empty() {
        return Err(format!(
            "GenerateCedarPolicy: missing required fields for entity '{entity_id}'"
        ));
    }

    let matrix: temper_authz::PolicyScopeMatrix =
        if let Some(matrix_val) = fields.get("scope_matrix") {
            serde_json::from_value(matrix_val.clone()).map_err(|e| {
                format!("GenerateCedarPolicy: invalid scope_matrix for entity '{entity_id}': {e}")
            })?
        } else {
            match scope {
                "narrow" => temper_authz::PolicyScopeMatrix {
                    principal: temper_authz::PrincipalScope::ThisAgent,
                    action: temper_authz::ActionScope::ThisAction,
                    resource: temper_authz::ResourceScope::ThisResource,
                    duration: temper_authz::DurationScope::Always,
                    agent_type_value: None,
                    role_value: None,
                    session_id: None,
                },
                "broad" => temper_authz::PolicyScopeMatrix {
                    principal: temper_authz::PrincipalScope::ThisAgent,
                    action: temper_authz::ActionScope::AllActionsOnType,
                    resource: temper_authz::ResourceScope::AnyOfType,
                    duration: temper_authz::DurationScope::Always,
                    agent_type_value: None,
                    role_value: None,
                    session_id: None,
                },
                _ => temper_authz::PolicyScopeMatrix::default_for(None),
            }
        };
    temper_authz::validate_policy_scope_matrix(&matrix).map_err(|e| {
        format!("GenerateCedarPolicy: invalid scope_matrix for entity '{entity_id}': {e}")
    })?;
    let principal_kind = fields
        .get("principal_kind")
        .and_then(|v| v.as_str())
        .unwrap_or("Agent");
    let generated_policy = temper_authz::generate_cedar_from_matrix(
        agent_id,
        principal_kind,
        action_name,
        resource_type,
        resource_id,
        &matrix,
    );

    tracing::info!(
        entity_id,
        tenant,
        scope,
        "GenerateCedarPolicy hook: generated policy, validating and loading"
    );

    let created_by = fields
        .get("decided_by")
        .and_then(|value| value.as_str())
        .unwrap_or("governance-decision");
    temper_server::authz::publish_policy_entry_generation(
        server,
        tenant,
        &format!("decision:{entity_id}"),
        &generated_policy,
        created_by,
    )
    .await?;

    tracing::info!(
        entity_id,
        "GenerateCedarPolicy hook: policy loaded successfully"
    );
    Ok(())
}
