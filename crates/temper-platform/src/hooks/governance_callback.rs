use sha2::{Digest, Sha256};
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
    source_entity_id: &str,
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

    let status = entity_fields
        .get("Status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let callback_action = match status {
        "Approved" => entity_fields
            .get("callback_on_approve")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "Denied" => entity_fields
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

    let params = if status == "Denied" {
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

    let tid = TenantId::new(callback_tenant);
    let entity_type = resolve_callback_entity_type(server, &tid, callback_entity_set);
    let mut agent_context = AgentContext::for_service("governance-service");
    agent_context.idempotency_key = Some(callback_idempotency_key(
        source_entity_id,
        status,
        callback_tenant,
        &entity_type,
        callback_entity_id,
        callback_action,
    ));
    let response = server
        .dispatch_tenant_action(
            &tid,
            &entity_type,
            callback_entity_id,
            callback_action,
            params,
            &agent_context,
        )
        .await
        .map_err(|error| {
            tracing::error!(
                error = %error,
                tenant = callback_tenant,
                entity_set = callback_entity_set,
                entity_type,
                entity_id = callback_entity_id,
                action = callback_action,
                "DispatchCallback: failed to dispatch callback action"
            );
            format!("DispatchCallback: failed to dispatch callback action: {error}")
        })?;
    if !response.success {
        let error = response
            .error
            .as_deref()
            .unwrap_or("callback action returned success=false without an error");
        tracing::error!(
            error,
            tenant = callback_tenant,
            entity_set = callback_entity_set,
            entity_type,
            entity_id = callback_entity_id,
            action = callback_action,
            "DispatchCallback: callback action was rejected"
        );
        return Err(format!(
            "DispatchCallback: callback action was rejected: {error}"
        ));
    }

    tracing::info!(
        tenant = callback_tenant,
        entity_set = callback_entity_set,
        entity_type,
        entity_id = callback_entity_id,
        action = callback_action,
        "DispatchCallback: callback action dispatched successfully"
    );

    Ok(())
}

fn callback_idempotency_key(
    source_entity_id: &str,
    status: &str,
    callback_tenant: &str,
    callback_entity_type: &str,
    callback_entity_id: &str,
    callback_action: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        "DispatchCallback",
        source_entity_id,
        status,
        callback_tenant,
        callback_entity_type,
        callback_entity_id,
        callback_action,
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("governance-callback:{:x}", hasher.finalize())
}

/// Resolve a callback target from an OData entity-set name to a governed entity type.
///
/// Callbacks are registered over HTTP and therefore store OData-facing entity set names
/// like `Sessions`. The dispatch layer, however, expects governed entity type names like
/// `Session`. If the callback already contains a type name, we preserve it.
fn resolve_callback_entity_type(
    server: &ServerState,
    tenant: &TenantId,
    entity_set_or_type: &str,
) -> String {
    {
        let registry = match server.registry.read() {
            Ok(registry) => registry,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entity_type) = registry.resolve_entity_type(tenant, entity_set_or_type) {
            return entity_type;
        }
        if registry.get_spec(tenant, entity_set_or_type).is_some() {
            return entity_set_or_type.to_string();
        }
    }
    server
        .entity_set_map
        .get(entity_set_or_type)
        .cloned()
        .or_else(|| {
            server
                .transition_tables
                .contains_key(entity_set_or_type)
                .then(|| entity_set_or_type.to_string())
        })
        .unwrap_or_else(|| entity_set_or_type.to_string())
}

#[cfg(test)]
#[path = "governance_callback_test.rs"]
mod tests;
