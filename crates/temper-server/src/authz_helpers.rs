//! Shared authorization helpers for Cedar policy enforcement.
//!
//! Extracts the common pattern for authorization checks and denial recording
//! used across OData bindings, policy management, spec submission, and WASM
//! authz gates.

use axum::http::HeaderMap;
use temper_authz::SecurityContext;
use temper_runtime::scheduler::sim_now;

use crate::state::{PendingDecision, ServerState, TrajectoryEntry};

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
/// Creates a `PendingDecision` for human review, pushes it to the bounded log
/// and broadcasts it via SSE. Also records a `TrajectoryEntry` with
/// `authz_denied: Some(true)` for the evolution engine.
///
/// Returns the `PendingDecision` so callers can include the decision ID in
/// their HTTP response.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_authz_denial(
    state: &ServerState,
    tenant: &str,
    security_ctx: &SecurityContext,
    agent_id_override: Option<&str>,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    resource_attrs: serde_json::Value,
    reason: &str,
    module_name: Option<String>,
) -> PendingDecision {
    let principal_id = agent_id_override.unwrap_or(security_ctx.principal.id.as_str());

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

    // Push to bounded log + broadcast for SSE.
    {
        let mut log = state.pending_decision_log.write().unwrap(); // ci-ok: infallible lock
        if log.push(pd.clone()) {
            let _ = state.pending_decision_tx.send(pd.clone());
        }
    }

    // Record trajectory for evolution engine.
    let traj = TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: resource_type.to_string(),
        entity_id: resource_id.to_string(),
        action: action.to_string(),
        success: false,
        from_status: None,
        to_status: None,
        error: Some(reason.to_string()),
        agent_id: Some(principal_id.to_string()),
        session_id: None,
        authz_denied: Some(true),
        denied_resource: Some(format!("{resource_type}:{resource_id}")),
        denied_module: None,
    };
    {
        let mut tlog = state.trajectory_log.write().unwrap(); // ci-ok: infallible lock
        tlog.push(traj);
    }

    pd
}
