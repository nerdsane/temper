//! Shared authorization helpers for Cedar policy enforcement.
//!
//! Extracts the common pattern for authorization checks and denial recording
//! used across OData bindings, policy management, spec submission, and WASM
//! authz gates.

use axum::http::HeaderMap;
use temper_authz::SecurityContext;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::dispatch::AgentContext;
use crate::state::{PendingDecision, TrajectoryEntry, TrajectorySource};

/// Extract `X-Temper-*` headers from an axum `HeaderMap` into `(key, value)` pairs
/// suitable for `SecurityContext::from_headers`.
pub(crate) fn extract_temper_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let key = name.as_str().to_lowercase();
            if key.starts_with("x-temper-") {
                value.to_str().ok().map(|v| (key, v.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Build a `SecurityContext` from request headers, optionally enriched with
/// agent identity from an `AgentContext`.
pub(crate) fn security_context_from_headers(
    headers: &HeaderMap,
    agent_id: Option<&str>,
    session_id: Option<&str>,
) -> SecurityContext {
    let temper_headers = extract_temper_headers(headers);
    SecurityContext::from_headers(&temper_headers).with_agent_context(agent_id, session_id)
}

/// Record result of an authorization denial.
///
/// Creates a `PendingDecision` for human review, broadcasts it via SSE, and
/// persists both the decision and trajectory to Turso synchronously.
///
/// Returns the `PendingDecision` so callers can include the decision ID in
/// their HTTP response.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_authz_denial(
    state: &crate::state::ServerState,
    tenant: &str,
    security_ctx: &SecurityContext,
    agent_id_override: Option<&str>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    resource_attrs: serde_json::Value,
    reason: &str,
    module_name: Option<String>,
    from_status: Option<String>,
) -> PendingDecision {
    let principal_id = agent_id_override.unwrap_or(security_ctx.principal.id.as_str());
    let denied_module = module_name.clone();
    let session_id = security_ctx
        .context_attrs
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let pd = PendingDecision::from_denial(
        tenant,
        principal_id,
        action,
        resource_type,
        resource_id,
        resource_attrs,
        reason,
        module_name,
    );

    // Broadcast for SSE.
    let _ = state.pending_decision_tx.send(pd.clone());

    // Persist decision to Turso synchronously.
    if let Err(e) = state.persist_pending_decision(&pd).await {
        eprintln!("Warning: failed to persist pending decision {}: {e}", pd.id);
    }

    // Also create a GovernanceDecision entity in the temper-system tenant.
    let gd_id = format!("GD-{}", sim_uuid());
    let gd_params = serde_json::json!({
        "tenant": tenant,
        "agent_id": principal_id,
        "action_name": action,
        "resource_type": resource_type,
        "resource_id": resource_id,
        "denial_reason": reason,
        "scope": "narrow",
        "pending_decision_id": pd.id,
    });
    let system_tenant = TenantId::new("temper-system");
    if let Err(e) = state
        .dispatch_tenant_action(
            &system_tenant,
            "GovernanceDecision",
            &gd_id,
            "CreateGovernanceDecision",
            gd_params,
            &AgentContext::default(),
        )
        .await
    {
        tracing::warn!(
            error = %e,
            "failed to create GovernanceDecision entity for denial"
        );
    }

    // Record trajectory to Turso synchronously.
    let traj = TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: resource_type.to_string(),
        entity_id: resource_id.to_string(),
        action: action.to_string(),
        success: false,
        from_status,
        to_status: None,
        error: Some(reason.to_string()),
        agent_id: Some(principal_id.to_string()),
        session_id,
        authz_denied: Some(true),
        denied_resource: Some(format!("{resource_type}:{resource_id}")),
        denied_module,
        source: Some(TrajectorySource::Authz),
        spec_governed: None,
    };
    if let Err(e) = state.persist_trajectory_entry(&traj).await {
        eprintln!("Warning: failed to persist authz trajectory: {e}");
    }

    pd
}
