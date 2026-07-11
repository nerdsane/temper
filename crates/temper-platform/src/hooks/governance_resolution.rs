//! Finalize GovernanceDecision only after its ordered effects succeed.

use temper_runtime::TenantId;
use temper_server::ServerState;
use temper_server::request_context::AgentContext;

use crate::bootstrap::SYSTEM_TENANT;

pub(super) async fn handle_finalize_governance_resolution(
    entity_type: &str,
    entity_id: &str,
    entity_fields: &serde_json::Value,
    server: &ServerState,
) -> Result<(), String> {
    if entity_type != "GovernanceDecision" {
        return Err(format!(
            "FinalizeGovernanceResolution is not valid for entity type {entity_type:?}"
        ));
    }
    let progress_status = entity_fields
        .get("Status")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!("FinalizeGovernanceResolution: missing Status for entity {entity_id:?}")
        })?;
    let (action, terminal_status) = match progress_status {
        "Approving" => ("FinalizeApproval", "Approved"),
        "Denying" => ("FinalizeDenial", "Denied"),
        other => {
            return Err(format!(
                "FinalizeGovernanceResolution: status {other:?} is not resolvable"
            ));
        }
    };

    let mut context = AgentContext::for_service("governance-service");
    context.idempotency_key = Some(format!(
        "governance-finalization:{entity_id}:{progress_status}"
    ));
    let response = server
        .dispatch_tenant_action(
            &TenantId::new(SYSTEM_TENANT),
            "GovernanceDecision",
            entity_id,
            action,
            serde_json::json!({}),
            &context,
        )
        .await
        .map_err(|error| format!("{action} dispatch failed: {error}"))?;
    if !response.success {
        return Err(response
            .error
            .unwrap_or_else(|| format!("{action} transition failed")));
    }
    if response.state.status != terminal_status {
        return Err(format!(
            "{action} returned status {:?}; expected {terminal_status:?}",
            response.state.status
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "governance_resolution_test.rs"]
mod tests;
