use temper_runtime::TenantId;
use temper_server::ServerState;
use temper_server::request_context::AgentContext;

/// Dispatch callback to a registered target entity.
///
/// Reads callback fields from GovernanceDecision entity state. If a callback
/// is registered (callback_tenant is non-empty), dispatches the appropriate
/// action on the target entity via cross-tenant dispatch.
///
/// This hook is also used for late callback registration: `RegisterCallback`
/// now triggers `DispatchCallback`, so replaying callback wiring after a
/// decision has already been approved or denied will immediately redeliver
/// the resolution to the waiting target entity.
pub(super) async fn handle_dispatch_callback(
    governance_decision_id: &str,
    entity_fields: &serde_json::Value,
    server: &ServerState,
) -> Result<(), String> {
    let callback_tenant = entity_fields
        .get("callback_tenant")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let callback_entity_set = entity_fields
        .get("callback_entity_set")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let callback_entity_id = entity_fields
        .get("callback_entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if callback_tenant.is_empty() || callback_entity_set.is_empty() || callback_entity_id.is_empty()
    {
        tracing::debug!("DispatchCallback: no callback registered — skipping");
        return Ok(());
    }
    let callback_capability = entity_fields
        .get("callback_capability")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "DispatchCallback: target-minted capability is missing".to_string())?;
    let capability = server
        .validate_governance_callback_binding(
            governance_decision_id,
            entity_fields,
            callback_capability,
        )
        .map_err(|error| format!("DispatchCallback: invalid callback capability: {error}"))?;

    let status = entity_fields
        .get("Status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let callback_action = match status {
        "Approving" | "Approved" => entity_fields
            .get("callback_on_approve")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "Denying" | "Denied" => entity_fields
            .get("callback_on_deny")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "Pending" => {
            tracing::debug!("DispatchCallback: decision still pending — skipping callback");
            return Ok(());
        }
        _ => {
            tracing::debug!(
                status,
                "DispatchCallback: decision not in a callback-emitting terminal state — skipping"
            );
            return Ok(());
        }
    };

    if callback_action.is_empty() {
        tracing::debug!(
            status,
            "DispatchCallback: no callback action for status — skipping"
        );
        return Ok(());
    }

    let terminal_status = match status {
        "Approving" | "Approved" => "Approved",
        "Denying" | "Denied" => "Denied",
        _ => unreachable!("callback status was matched above"),
    };
    let params = if terminal_status == "Denied" {
        serde_json::json!({"error_message": "Action denied by human reviewer"})
    } else {
        serde_json::json!({})
    };

    tracing::info!(
        callback_tenant,
        callback_entity_set,
        callback_entity_id,
        callback_action,
        "DispatchCallback: dispatching callback"
    );

    let callback_idempotency_key = format!("{}:{terminal_status}", capability.delivery_id);
    let tenant = TenantId::new(callback_tenant);
    let entity_type = capability.target_entity_type;
    let mut context = AgentContext::for_service("governance-service");
    context.idempotency_key = Some(callback_idempotency_key);
    let response = server
        .dispatch_tenant_action(
            &tenant,
            &entity_type,
            callback_entity_id,
            callback_action,
            params,
            &context,
        )
        .await
        .map_err(|error| {
            format!(
                "DispatchCallback: failed to dispatch {callback_entity_set}/{callback_entity_id}.{callback_action}: {error}"
            )
        })?;
    if !response.success {
        return Err(format!(
            "DispatchCallback: target action failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown callback failure".to_string())
        ));
    }
    tracing::info!(
        callback_tenant,
        callback_entity_set,
        entity_type,
        callback_entity_id,
        callback_action,
        "DispatchCallback: callback action dispatched successfully"
    );

    Ok(())
}

#[cfg(test)]
#[path = "governance_callback_test.rs"]
mod tests;
