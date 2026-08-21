//! Shared authorization helpers for Cedar policy enforcement.
//!
//! Extracts the common pattern for authorization checks and denial recording
//! used across OData bindings, policy management, spec submission, and WASM
//! authz gates.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::request_context::AgentContext;
use crate::state::{PendingDecision, TrajectoryEntry, TrajectorySource};

/// Require the credential-derived request context installed by the auth edge.
///
/// Protected handlers use `Option<Extension<_>>` so a missing edge context is
/// an explicit `401` rather than axum's generic missing-extension rejection.
pub(crate) fn require_authenticated_context(
    authenticated: Option<&AuthenticatedRequestContext>,
) -> Result<&AuthenticatedRequestContext, StatusCode> {
    authenticated.ok_or(StatusCode::UNAUTHORIZED)
}

/// Ensure a tenant path parameter matches the credential-bound tenant.
///
/// The path remains useful for route shape, but it is never an authority input.
/// A credential resolved for one tenant cannot operate on another tenant's
/// path-scoped resource.
pub(crate) fn require_tenant_match<'a>(
    authenticated: &'a AuthenticatedRequestContext,
    requested_tenant: &str,
) -> Result<&'a TenantId, StatusCode> {
    if authenticated.tenant().as_str() != requested_tenant {
        tracing::warn!(
            authenticated_tenant = %authenticated.tenant(),
            requested_tenant,
            "credential tenant does not match protected route tenant"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(authenticated.tenant())
}

/// Check Cedar authorization for observe endpoints.
///
/// Every principal must have the specified `action` on `resource_type`.
/// System authority is represented by the built-in Cedar policy, not a caller
/// side channel. Returns `Ok(())` if authorized or
/// `Err(StatusCode::FORBIDDEN)` if denied.
pub(crate) fn require_observe_auth(
    state: &crate::state::ServerState,
    authenticated: &AuthenticatedRequestContext,
    action: &str,
    resource_type: &str,
) -> Result<(), axum::http::StatusCode> {
    let security_ctx = authenticated.security_context();
    if let Err(denial) = state.authorize_with_context(
        security_ctx,
        action,
        resource_type,
        &BTreeMap::new(),
        authenticated.tenant().as_str(),
    ) {
        tracing::warn!(reason = %denial, action, resource_type, "unauthorized observe access");
        return Err(axum::http::StatusCode::FORBIDDEN);
    }
    Ok(())
}

/// Resolve the tenant scope for an observe endpoint.
///
/// Every credential is tenant-bound, including operator credentials. Observe
/// handlers therefore always filter to the authenticated tenant; a cross-tenant
/// view requires an explicit higher-level aggregation rather than an absent
/// tenant header.
#[allow(dead_code)] // False positive: used by observe/ handlers via crate::authz re-export
pub(crate) fn observe_tenant_scope(authenticated: &AuthenticatedRequestContext) -> &TenantId {
    authenticated.tenant()
}

/// One exact Cedar resource check derived from authenticated request authority.
#[cfg(feature = "observe")]
pub(crate) struct ResourceAuthorization<'a> {
    pub action: &'a str,
    pub resource_type: &'a str,
    pub resource_id: &'a str,
    pub resource_attrs: BTreeMap<String, serde_json::Value>,
}

/// Require Cedar authorization for one credential-tenant resource.
///
/// The tenant and principal always come from [`AuthenticatedRequestContext`].
/// Callers may add resource-specific attributes, while this helper guarantees
/// that Cedar receives the exact resource id used by the operation.
#[cfg(feature = "observe")]
pub(crate) fn require_resource_authorization(
    state: &crate::state::ServerState,
    authenticated: &AuthenticatedRequestContext,
    mut input: ResourceAuthorization<'_>,
) -> Result<(), StatusCode> {
    input.resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(input.resource_id.to_string()),
    );
    if let Err(denial) = state.authorize_with_context(
        authenticated.security_context(),
        input.action,
        input.resource_type,
        &input.resource_attrs,
        authenticated.tenant().as_str(),
    ) {
        tracing::warn!(
            reason = %denial,
            tenant = %authenticated.tenant(),
            principal_id = %authenticated.security_context().principal.id,
            action = input.action,
            resource_type = input.resource_type,
            resource_id = input.resource_id,
            "credential-bound resource authorization denied"
        );
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
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
    /// Whether the denied operation was a spec-governed dispatch.
    ///
    /// `Some(false)` marks rows the conformance checker must never judge:
    /// pre-flight probes and management-plane denials, whose entity type,
    /// action, and session can be caller-chosen. Left `None` only by the OData
    /// entity-dispatch guards, where the row describes a genuine attempted
    /// dispatch of a registered action (`crate::conformance::walk::row_disposition`
    /// walks `None` as actor execution, so `None` is an assertion that the row
    /// belongs in a run's verdict).
    pub spec_governed: Option<bool>,
    /// Session the gate resolved for this request, when it had one.
    ///
    /// Passed in rather than recomputed: the caller-declared session rides the
    /// request context now (it must never reach Cedar), so recomputing it from
    /// `context_attrs` would leave exactly the session-scoped denials the gate
    /// admitted with no session on their record.
    pub session_id: Option<String>,
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
    authenticated: &AuthenticatedRequestContext,
    mut input: GovernedMutationAuth<'_>,
) -> Option<(StatusCode, String)> {
    if require_tenant_match(authenticated, input.tenant).is_err() {
        return Some((
            StatusCode::UNAUTHORIZED,
            "credential tenant does not match protected route tenant".to_string(),
        ));
    }
    let security_ctx = authenticated.security_context();
    input
        .resource_attrs
        .entry("id".to_string())
        .or_insert_with(|| serde_json::Value::String(input.resource_id.to_string()));

    let Err(denial) = state.authorize_with_context(
        security_ctx,
        input.action,
        input.resource_type,
        &input.resource_attrs,
        input.tenant,
    ) else {
        return None;
    };

    let reason = denial.to_string();
    // Either source is valid here, and neither is an authorization input: the
    // request context carries the caller's declared session (telemetry only),
    // while `context_attrs` carries one a server-side path minted itself — the
    // WASM/agent dispatch contexts do this, and their session is not a header.
    let session_id = authenticated.session_id().or_else(|| {
        security_ctx
            .context_attrs
            .get("sessionId")
            .and_then(|value| value.as_str())
    });
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
                security_ctx,
                agent_id_override: None,
                action: input.action,
                resource_type: input.resource_type,
                resource_id: input.resource_id,
                resource_attrs: resource_attrs_json,
                reason: &reason,
                module_name: input.module_name.map(str::to_string),
                from_status: input.from_status.map(str::to_string),
                intent: authenticated.intent().map(str::to_string),
                session_id: session_id.map(str::to_string),
                // Management-plane denial, not a spec-governed dispatch.
                spec_governed: Some(false),
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
    let session_id = input.session_id.clone().or_else(|| {
        input
            .security_ctx
            .context_attrs
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

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
        spec_governed: input.spec_governed,
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
    use temper_authz::AuthenticatedRequestContext;

    #[test]
    fn missing_authenticated_context_is_unauthorized() {
        assert_eq!(
            require_authenticated_context(None).unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn typed_context_preserves_resolved_identity_and_tenant() {
        let authenticated =
            AuthenticatedRequestContext::new(TenantId::new("tenant-a"), SecurityContext::system());

        let resolved = require_authenticated_context(Some(&authenticated)).unwrap();

        assert_eq!(resolved.tenant().as_str(), "tenant-a");
        assert_eq!(resolved.security_context().principal.id, "system");
    }

    #[test]
    fn tenant_mismatch_is_unauthorized() {
        let authenticated =
            AuthenticatedRequestContext::new(TenantId::new("tenant-a"), SecurityContext::system());

        assert_eq!(
            require_tenant_match(&authenticated, "tenant-b").unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }
}
