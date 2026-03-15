//! Shared authorization helpers for Cedar policy enforcement.
//!
//! Extracts the common pattern for authorization checks and denial recording
//! used across OData bindings, policy management, spec submission, and WASM
//! authz gates.

use axum::http::HeaderMap;
use temper_authz::SecurityContext;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::request_context::AgentContext;
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
    agent_type: Option<&str>,
) -> SecurityContext {
    let temper_headers = extract_temper_headers(headers);
    SecurityContext::from_headers(&temper_headers)
        .with_agent_context(agent_id, session_id, agent_type)
}

/// Check Cedar authorization for observe endpoints.
///
/// Admin and System principals bypass the check. Other principals must have the
/// specified `action` on `resource_type`. Returns `Ok(())` if authorized or
/// `Err(StatusCode::FORBIDDEN)` if denied.
pub(crate) fn require_observe_auth(
    state: &crate::state::ServerState,
    headers: &HeaderMap,
    action: &str,
    resource_type: &str,
) -> Result<(), axum::http::StatusCode> {
    let security_ctx = security_context_from_headers(headers, None, None, None);
    if matches!(
        security_ctx.principal.kind,
        temper_authz::PrincipalKind::Admin | temper_authz::PrincipalKind::System
    ) {
        return Ok(());
    }
    let tenant = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("system");
    if let Err(denial) = state.authorize_with_context(
        &security_ctx,
        action,
        resource_type,
        &std::collections::BTreeMap::new(),
        tenant,
    ) {
        tracing::warn!(reason = %denial, action, resource_type, "unauthorized observe access");
        return Err(axum::http::StatusCode::FORBIDDEN);
    }
    Ok(())
}

/// Resolve the tenant scope for an observe endpoint.
///
/// Returns `Some(tenant)` when results should be filtered to a single tenant,
/// or `None` when the caller is authorized for a cross-tenant admin view.
///
/// - If `X-Tenant-Id` is present → filter to that tenant.
/// - If missing and principal is Admin/System → cross-tenant view (`None`).
/// - If missing in multi-tenant mode for non-admin → `403 Forbidden`.
#[allow(dead_code)] // False positive: used by observe/ handlers via crate::authz re-export
pub(crate) fn observe_tenant_scope(
    state: &crate::state::ServerState,
    headers: &axum::http::HeaderMap,
) -> Result<Option<TenantId>, axum::http::StatusCode> {
    // If the caller provided an explicit tenant, always scope to it.
    if let Some(val) = headers.get("x-tenant-id")
        && let Ok(s) = val.to_str()
        && !s.is_empty()
    {
        return Ok(Some(TenantId::new(s)));
    }

    // No tenant header — admin/system get cross-tenant view.
    let security_ctx = security_context_from_headers(headers, None, None, None);
    if matches!(
        security_ctx.principal.kind,
        temper_authz::PrincipalKind::Admin | temper_authz::PrincipalKind::System
    ) {
        return Ok(None);
    }

    // Non-admin without tenant in multi-tenant mode: reject.
    if !state.single_tenant_mode {
        tracing::warn!(
            principal = %security_ctx.principal.id,
            "non-admin observe request without X-Tenant-Id in multi-tenant mode"
        );
        return Err(axum::http::StatusCode::FORBIDDEN);
    }

    // Single-tenant compat: cross-tenant view for all principals.
    Ok(None)
}

/// Input for recording an authorization denial.
pub(crate) struct DenialInput<'a> {
    /// Tenant where the denial occurred.
    pub tenant: &'a str,
    /// Security context of the requester.
    pub security_ctx: &'a SecurityContext,
    /// Override the principal ID (e.g., with agent ID).
    pub agent_id_override: Option<&'a str>,
    /// Action that was denied.
    pub action: &'a str,
    /// Resource type being accessed.
    pub resource_type: &'a str,
    /// Resource identifier.
    pub resource_id: &'a str,
    /// Additional resource attributes for the decision record.
    pub resource_attrs: serde_json::Value,
    /// Human-readable denial reason.
    pub reason: &'a str,
    /// WASM module name (if denial occurred in a WASM gate).
    pub module_name: Option<String>,
    /// Entity status at the time of denial.
    pub from_status: Option<String>,
}

/// Record result of an authorization denial.
///
/// Creates a `PendingDecision` for human review, broadcasts it via SSE, and
/// persists both the decision and trajectory to Turso synchronously.
///
/// Returns the `PendingDecision` so callers can include the decision ID in
/// their HTTP response.
pub(crate) async fn record_authz_denial(
    state: &crate::state::ServerState,
    input: DenialInput<'_>,
) -> PendingDecision {
    let principal_id = input
        .agent_id_override
        .unwrap_or(input.security_ctx.principal.id.as_str());
    let denied_module = input.module_name.clone();
    let session_id = input
        .security_ctx
        .context_attrs
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut pd = PendingDecision::from_denial(
        input.tenant,
        principal_id,
        input.action,
        input.resource_type,
        input.resource_id,
        input.resource_attrs,
        input.reason,
        input.module_name,
    );
    pd.agent_type = input.security_ctx.principal.agent_type.clone();
    pd.principal_kind = Some(
        match input.security_ctx.principal.kind {
            temper_authz::PrincipalKind::Customer => "Customer",
            temper_authz::PrincipalKind::Agent => "Agent",
            temper_authz::PrincipalKind::Admin => "Admin",
            temper_authz::PrincipalKind::System => "System",
        }
        .to_string(),
    );
    pd.session_id = session_id.clone();

    // Broadcast for SSE.
    let _ = state.pending_decision_tx.send(pd.clone());

    // Persist decision to Turso synchronously.
    if let Err(e) = state.persist_pending_decision(&pd).await {
        tracing::warn!(error = %e, id = %pd.id, "failed to persist pending decision");
    }

    // Also create a GovernanceDecision entity in the temper-system tenant.
    let gd_id = format!("GD-{}", sim_uuid());
    let gd_params = serde_json::json!({
        "tenant": input.tenant,
        "agent_id": principal_id,
        "action_name": input.action,
        "resource_type": input.resource_type,
        "resource_id": input.resource_id,
        "denial_reason": input.reason,
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
            &AgentContext::system(),
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
        tenant: input.tenant.to_string(),
        entity_type: input.resource_type.to_string(),
        entity_id: input.resource_id.to_string(),
        action: input.action.to_string(),
        success: false,
        from_status: input.from_status,
        to_status: None,
        error: Some(input.reason.to_string()),
        agent_id: Some(principal_id.to_string()),
        session_id,
        authz_denied: Some(true),
        denied_resource: Some(format!("{}:{}", input.resource_type, input.resource_id)),
        denied_module,
        source: Some(TrajectorySource::Authz),
        spec_governed: None,
        agent_type: input.security_ctx.principal.agent_type.clone(),
    };
    if let Err(e) = state.persist_trajectory_entry(&traj).await {
        tracing::warn!(error = %e, "failed to persist authz trajectory");
    }

    // Feed denial into suggestion engine for pattern detection.
    if let Ok(mut engine) = state.suggestion_engine.write() {
        engine.record_denial(
            traj.agent_type.as_deref(),
            input.action,
            input.resource_type,
            input.resource_id,
            &traj.timestamp,
        );
    }

    pd
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderValue as AxumHeaderValue;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::Value;
    use temper_authz::PrincipalKind;

    #[test]
    fn extract_temper_headers_filters_correctly() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-temper-principal"),
            HeaderValue::from_static("agent-007"),
        );
        headers.insert(
            HeaderName::from_static("x-temper-session"),
            HeaderValue::from_static("sess-123"),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer tok"),
        );

        let result = extract_temper_headers(&headers);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&("x-temper-principal".to_string(), "agent-007".to_string())));
        assert!(result.contains(&("x-temper-session".to_string(), "sess-123".to_string())));
    }

    #[test]
    fn extract_temper_headers_empty() {
        let headers = HeaderMap::new();
        assert!(extract_temper_headers(&headers).is_empty());
    }

    #[test]
    fn extract_temper_headers_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-temper-test"),
            HeaderValue::from_static("val"),
        );
        let result = extract_temper_headers(&headers);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "x-temper-test");
    }

    #[test]
    fn security_context_from_headers_preserves_agent_type_from_http_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-temper-principal-id",
            AxumHeaderValue::from_static("bot-1"),
        );
        headers.insert(
            "x-temper-principal-kind",
            AxumHeaderValue::from_static("agent"),
        );
        headers.insert(
            "x-temper-agent-type",
            AxumHeaderValue::from_static("supervisor"),
        );

        let ctx = security_context_from_headers(&headers, None, None, None);
        assert_eq!(ctx.principal.kind, PrincipalKind::Agent);
        assert_eq!(ctx.principal.id, "bot-1");
        assert_eq!(ctx.principal.agent_type.as_deref(), Some("supervisor"));
    }

    #[test]
    fn security_context_from_headers_preserves_principal_attrs_from_http_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-temper-principal-id",
            AxumHeaderValue::from_static("bot-1"),
        );
        headers.insert(
            "x-temper-principal-kind",
            AxumHeaderValue::from_static("agent"),
        );
        headers.insert(
            "x-temper-attr-region",
            AxumHeaderValue::from_static("us-east-1"),
        );

        let ctx = security_context_from_headers(&headers, None, None, None);
        assert_eq!(
            ctx.principal.attributes.get("region"),
            Some(&Value::String("us-east-1".to_string()))
        );
    }
}
