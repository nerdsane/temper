//! Shared authorization helpers for Cedar policy enforcement.
//!
//! Extracts the common pattern for authorization checks and denial recording
//! used across OData bindings, policy management, spec submission, and WASM
//! authz gates.

use std::collections::BTreeMap;

use axum::http::{HeaderMap, StatusCode};
use temper_authz::SecurityContext;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::request_context::{AgentContext, intent_from_headers};
use crate::state::{PendingDecision, TrajectoryEntry, TrajectorySource};

/// Extract `X-Temper-*` headers from an axum `HeaderMap` into `(key, value)` pairs
/// suitable for `SecurityContext::from_headers`.
pub(crate) fn extract_temper_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let key = name.as_str().to_lowercase();
            if key == "x-temper-action-context" {
                None
            } else if key.starts_with("x-temper-") {
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

/// Check Cedar authorization for an endpoint that returns recorded agent
/// content, with no principal-kind bypass.
///
/// [`require_observe_auth`] lets an Admin or System principal past Cedar
/// without a policy, and the principal kind is read straight off the request
/// headers ([`temper_authz::SecurityContext::from_headers`]) because the
/// platform has no request authentication in front of it yet. That combination
/// is survivable for aggregate counters. It is not survivable for a surface
/// that returns one named run's prompts, tool results, and request bodies, so
/// these endpoints do not use it: every caller must be permitted by a Cedar
/// policy in the tenant, whatever kind it declares itself to be.
///
/// System is not reachable here — `from_headers` refuses to build a System
/// principal precisely to stop header-declared escalation — and platform code
/// paths that legitimately act as System are covered by the built-in
/// `system-platform` permit rather than by a bypass in this function.
///
/// # What this does not fix
///
/// The principal itself is still self-declared: [`SecurityContext`] is built
/// from `X-Temper-*` request headers ([`temper_authz::SecurityContext::from_headers`],
/// consumed at `crate::state::ServerState::authorize_with_context`), and no
/// component in front of this one authenticates them. A Cedar permit therefore
/// binds a claimed principal id, not a verified one. This function is the
/// strictest gate the platform has today, not a sufficient one.
///
/// Closing that gap is ARN-255 — a platform-run authorization server issuing
/// verified principals — and it is a platform-wide change to how every request
/// is authenticated, not something these endpoints can do locally. Gating the
/// pre-existing OTS routes (`POST /api/ots/trajectories`,
/// `GET /api/ots/trajectories`, which carry no authorization check at all) is
/// ARN-187, in flight on `claude/arn-187-ots-auth-gate`; duplicating that gate
/// here would collide with it on merge. Neither is rebuilt on this branch.
pub(crate) fn require_trajectory_content_auth(
    state: &crate::state::ServerState,
    headers: &HeaderMap,
    action: &str,
    resource_type: &str,
    tenant: &str,
) -> Result<(), StatusCode> {
    let security_ctx = security_context_from_headers(headers, None, None, None);
    if let Err(denial) = state.authorize_with_context(
        &security_ctx,
        action,
        resource_type,
        &BTreeMap::new(),
        tenant,
    ) {
        tracing::warn!(
            reason = %denial,
            action,
            resource_type,
            tenant,
            principal_kind = ?security_ctx.principal.kind,
            "unauthorized trajectory content access"
        );
        return Err(StatusCode::FORBIDDEN);
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
    /// Caller-supplied intent (`X-Intent`) for the denied request.
    ///
    /// A denial without the intent behind it tells the Evolution Engine what
    /// was blocked but not what the agent was trying to accomplish, which is
    /// the half that drives policy proposals.
    pub intent: Option<String>,
}

/// Input for a resumable management mutation authorization check.
#[allow(unused)] // Staged for governed management endpoints after the latency package lands.
pub(crate) struct GovernedMutationAuth<'a> {
    pub tenant: &'a str,
    pub action: &'a str,
    pub resource_type: &'a str,
    pub resource_id: &'a str,
    pub resource_attrs: BTreeMap<String, serde_json::Value>,
    pub module_name: Option<&'a str>,
    pub from_status: Option<&'a str>,
}

/// Authorize a resumable management mutation and record agent denials.
///
/// This helper is for mutating HTTP endpoints whose denied operation can be
/// retried after a human approval. Agent requests with session context become
/// `PendingDecision` records. Non-agent or sessionless denials stay ordinary
/// `403 Forbidden` responses so passive/admin surfaces do not generate noisy
/// approval work.
#[allow(unused)] // Staged for governed management endpoints after the latency package lands.
pub(crate) async fn require_governed_mutation_auth(
    state: &crate::state::ServerState,
    headers: &HeaderMap,
    mut input: GovernedMutationAuth<'_>,
) -> Option<(StatusCode, String)> {
    let security_ctx = security_context_from_headers(headers, None, None, None);
    if matches!(
        security_ctx.principal.kind,
        temper_authz::PrincipalKind::Admin
    ) {
        return None;
    }

    input
        .resource_attrs
        .entry("id".to_string())
        .or_insert_with(|| serde_json::Value::String(input.resource_id.to_string()));

    let Err(denial) = state.authorize_with_context(
        &security_ctx,
        input.action,
        input.resource_type,
        &input.resource_attrs,
        input.tenant,
    ) else {
        return None;
    };

    let reason = denial.to_string();
    let session_id = security_ctx
        .context_attrs
        .get("sessionId")
        .and_then(|v| v.as_str());
    if matches!(
        security_ctx.principal.kind,
        temper_authz::PrincipalKind::Agent
    ) && session_id.is_some()
    {
        let resource_attrs_json =
            serde_json::to_value(&input.resource_attrs).unwrap_or_else(|_| serde_json::json!({}));
        let pd = record_authz_denial(
            state,
            DenialInput {
                tenant: input.tenant,
                security_ctx: &security_ctx,
                agent_id_override: None,
                action: input.action,
                resource_type: input.resource_type,
                resource_id: input.resource_id,
                resource_attrs: resource_attrs_json,
                reason: &reason,
                module_name: input.module_name.map(str::to_string),
                from_status: input.from_status.map(str::to_string),
                intent: intent_from_headers(headers),
            },
        )
        .await;
        return Some((
            StatusCode::FORBIDDEN,
            serde_json::json!({
                "decision_id": pd.id,
                "error": {
                    "code": "AuthorizationDenied",
                    "message": format!("{reason} Decision {}", pd.id),
                }
            })
            .to_string(),
        ));
    }

    tracing::warn!(
        reason = %reason,
        action = input.action,
        resource_type = input.resource_type,
        resource_id = input.resource_id,
        "unauthorized non-resumable governed mutation"
    );
    Some((StatusCode::FORBIDDEN, reason))
}

/// Record result of an authorization denial.
///
/// Creates a `PendingDecision` for human review, broadcasts it via SSE, and
/// persists both the decision and trajectory to the durable metadata backend.
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
    let denial_request_body = input.resource_attrs.clone();
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

    // Persist decision synchronously.
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
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
    {
        tracing::warn!(
            error = %e,
            "failed to create GovernanceDecision entity for denial"
        );
    }
    pd.governance_decision_id = Some(gd_id.clone());

    // Re-persist with governance_decision_id link so approve/deny endpoints can find the GD.
    if let Err(e) = state.persist_pending_decision(&pd).await {
        tracing::warn!(error = %e, id = %pd.id, "failed to persist PD with governance_decision_id");
    }

    // Tell the Observe UI a new decision exists so the Decisions tab refreshes live.
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Decisions);

    // Record authorization denials as observability without back-pressuring the
    // caller — the trajectory entry below is enqueued onto the bounded outbox
    // and persisted by the drainer task (ADR-0067).
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
        // The Cedar-evaluated resource attributes are the request payload for
        // an authorization decision; without them a denial cannot be replayed
        // against a revised policy.
        request_body: Some(denial_request_body),
        intent: input.intent.clone(),
        matched_policy_ids: None,
        capture_seq: None,
    };
    if !state.enqueue_trajectory_entry(traj.clone()) {
        tracing::warn!("failed to enqueue authz trajectory");
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
    if let Some(store) = state.metadata_store_for_tenant(input.tenant).await
        && let Err(e) = store
            .upsert_policy_denial_pattern(
                input.tenant,
                traj.agent_type.as_deref(),
                input.action,
                input.resource_type,
                input.resource_id,
                &traj.timestamp,
            )
            .await
    {
        tracing::warn!(error = %e, tenant = input.tenant, backend = store.backend_name(), "failed to persist denial pattern");
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

    #[test]
    fn security_context_from_headers_drops_action_context_from_http_headers() {
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
            "x-temper-action-context",
            AxumHeaderValue::from_static("composite:App.Fork"),
        );

        let ctx = security_context_from_headers(&headers, None, None, None);
        assert!(!ctx.principal.attributes.contains_key("action_context"));
    }
}
