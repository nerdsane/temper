//! Decision management API endpoints.
//!
//! Handles listing, approving, and denying evolution decisions, plus SSE
//! streaming for real-time decision notifications (both per-tenant and
//! credential-tenant views).

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_evolution::records::{Decision, DecisionRecord, RecordHeader, RecordType};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use temper_runtime::tenant::TenantId;

use super::{PolicyAuthed, decisions_access, empty_decision_list, format_decision_list};
use crate::authz::{
    record_policy_change, require_authenticated_context, require_observe_auth, require_tenant_match,
};
use crate::request_context::AgentContext;
use crate::state::{DecisionStatus, PendingDecision, ServerState};

mod approval;
mod streams;
pub(crate) use streams::{
    handle_agent_progress_stream, handle_all_decisions_stream, handle_decision_stream,
};

/// Query parameters for listing decisions.
#[derive(serde::Deserialize)]
pub(crate) struct DecisionListParams {
    /// Optional status filter: "pending", "approved", "denied", "expired".
    status: Option<String>,
}

/// Body for approve request.
#[derive(serde::Deserialize)]
pub(crate) struct ApproveBody {
    /// Policy scope matrix for Cedar generation.
    scope: temper_authz::PolicyScopeMatrix,
}

/// GET /api/tenants/{tenant}/decisions — list decisions with optional status filter.
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/decisions"))]
pub(crate) async fn handle_list_decisions(
    State(state): State<ServerState>,
    Path(path_tenant): Path<String>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(params): Query<DecisionListParams>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if let Err(status) = require_tenant_match(authenticated, &path_tenant) {
        return status.into_response();
    }
    let tenant = authenticated.tenant().as_str();
    let access = match decisions_access::decision_list_access(&state, authenticated).await {
        Ok(access) => access,
        Err(resp) => return resp,
    };
    if let Some(store) = state.metadata_store_for_tenant(tenant).await {
        match store
            .query_decisions(tenant, params.status.as_deref())
            .await
        {
            Ok(data_strings) => return format_decision_list(access.filter(data_strings)),
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), "failed to query decisions");
            }
        }
    }
    empty_decision_list()
}

/// POST /api/tenants/{tenant}/decisions/{id}/approve — approve with scope.
#[instrument(skip_all, fields(tenant, id, otel.name = "POST /api/tenants/{tenant}/decisions/{id}/approve"))]
pub(crate) async fn handle_approve_decision(
    State(state): State<ServerState>,
    Path((_tenant, id)): Path<(String, String)>,
    auth: PolicyAuthed,
    axum::Json(body): axum::Json<ApproveBody>,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str().to_string();
    let decided_by = auth.security_context().principal.id.clone();
    let approval_guard = state.policy_approval_lock.lock().await;
    let scope = body.scope;
    if let Err(e) = temper_authz::validate_policy_scope_matrix(&scope) {
        return (
            StatusCode::BAD_REQUEST,
            format!("Invalid policy scope matrix: {e}"),
        )
            .into_response();
    }

    // Read decision from the durable metadata backend.
    let mut decision: PendingDecision = {
        let Some(store) = state.metadata_store_for_tenant(&tenant).await else {
            tracing::error!("durable metadata backend not configured for approve decision");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "durable metadata backend not configured",
            )
                .into_response();
        };
        match store.get_pending_decision(&tenant, &id).await {
            Ok(Some(data_str)) => match serde_json::from_str::<PendingDecision>(&data_str) {
                Ok(d) if d.tenant == tenant => d,
                _ => {
                    tracing::warn!("decision not found for approval");
                    return (StatusCode::NOT_FOUND, "Decision not found").into_response();
                }
            },
            Ok(None) => {
                tracing::warn!("decision not found for approval");
                return (StatusCode::NOT_FOUND, "Decision not found").into_response();
            }
            Err(e) => {
                tracing::error!(error = %e, backend = store.backend_name(), "failed to load decision");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to load decision: {e}"),
                )
                    .into_response();
            }
        }
    };

    if decision.status != DecisionStatus::Pending {
        tracing::warn!(status = ?decision.status, "decision already resolved");
        return (
            StatusCode::CONFLICT,
            format!("Decision already resolved as {:?}", decision.status),
        )
            .into_response();
    }

    if let Some(resp) = decisions_access::reject_self_resolution(&decided_by, &decision) {
        return resp;
    }

    let generated_policy = decision.generate_policy_from_matrix(&scope);
    let evolution_record_id = decision.evolution_record_id.clone();
    let pending_decision_json = match serde_json::to_string(&decision) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!(%error, "failed to serialize pending decision before approval");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize pending decision: {error}"),
            )
                .into_response();
        }
    };

    // Validate the generated policy combined with existing enabled policies.
    let prospective = {
        let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let existing = policies.get(&tenant).cloned().unwrap_or_default();
        if existing.is_empty() {
            generated_policy.clone()
        } else {
            format!("{existing}\n{generated_policy}")
        }
    };
    if let Err(error) = state.authz.validate_tenant_policies(&prospective) {
        tracing::warn!(%error, "policy validation failed");
        return (
            StatusCode::BAD_REQUEST,
            format!("Policy validation failed: {error}"),
        )
            .into_response();
    }

    // Construct the durable approved form before starting the transaction.
    decision.status = DecisionStatus::Approved;
    decision.approved_scope = Some(scope.clone());
    decision.generated_policy = Some(generated_policy.clone());
    decision.decided_by = Some(decided_by.clone());
    decision.decided_at = Some(sim_now().to_rfc3339());
    let approved_decision = decision.clone();
    let approved_decision_json = match serde_json::to_string(&approved_decision) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!(%error, "failed to serialize approved decision");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize approved decision: {error}"),
            )
                .into_response();
        }
    };
    let policy_id = format!("decision:{id}");
    let Some(store) = state.metadata_store_for_tenant(&tenant).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable metadata backend not configured",
        )
            .into_response();
    };
    let existing_policy_text = {
        let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        policies.get(&tenant).cloned().unwrap_or_default()
    };
    let result = approval::commit_then_activate(
        || async {
            store
                .commit_policy_approval(crate::storage::PolicyApprovalCommit {
                    tenant: &tenant,
                    decision_id: &id,
                    approved_decision_json: &approved_decision_json,
                    policy_id: &policy_id,
                    cedar_text: &generated_policy,
                    created_by: &decided_by,
                })
                .await
                .map_err(|error| error.to_string())
        },
        || {
            state
                .authz
                .reload_tenant_policies(&tenant, &prospective)
                .map_err(|error| error.to_string())?;
            match state.tenant_policies.write() {
                Ok(mut policies) => {
                    policies.insert(tenant.clone(), prospective.clone());
                    Ok(())
                }
                Err(error) => {
                    let rollback_runtime = state
                        .authz
                        .reload_tenant_policies(&tenant, &existing_policy_text)
                        .err()
                        .map(|rollback| format!("; runtime rollback failed: {rollback}"))
                        .unwrap_or_default();
                    Err(format!(
                        "tenant policy cache lock poisoned: {error}{rollback_runtime}"
                    ))
                }
            }
        },
        || async {
            store
                .rollback_policy_approval(&tenant, &id, &pending_decision_json, &policy_id)
                .await
                .map_err(|error| error.to_string())
        },
    )
    .await;
    if let Err(error) = result {
        tracing::error!(decision_id = %id, %error, "policy approval failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    drop(approval_guard);
    record_policy_change(&state, &tenant, &policy_id, &decided_by);
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Decisions);

    // Create D-Record for the approval (evolution audit trail).
    // Link to the A-Record via derived_from for O-A-D chain tracing.
    let d_header = RecordHeader::new(RecordType::Decision, "human:approval");
    let d_header = match evolution_record_id {
        Some(ref eid) => d_header.derived_from(eid.clone()),
        None => d_header,
    };
    let d_record = DecisionRecord {
        header: d_header,
        decision: Decision::Approved,
        decided_by: decided_by.clone(),
        rationale: format!(
            "Approved with scope: {:?}. Policy: {}",
            scope, generated_policy
        ),
        verification_results: None,
        implementation: None,
    };
    // Persist D-Record to the platform metadata backend.
    if let Some(store) = state.metadata_store_for_tenant(&tenant).await {
        let data_json = serde_json::to_string(&d_record).unwrap_or_default();
        if let Err(e) = store
            .insert_evolution_record(crate::storage::EvolutionRecordWrite {
                tenant: &tenant,
                id: &d_record.header.id,
                record_type: "Decision",
                status: &format!("{:?}", d_record.header.status),
                created_by: &d_record.header.created_by,
                derived_from: d_record.header.derived_from.as_deref(),
                data_json: &data_json,
            })
            .await
        {
            tracing::warn!(error = %e, backend = store.backend_name(), "failed to persist D-Record");
        }
    }

    // Dispatch GovernanceDecision.Approve — triggers DispatchCallback effect
    // which resumes/fails the waiting Session via the registered callback.
    if let Some(ref gd_id) = approved_decision.governance_decision_id {
        let state_c = state.clone();
        let gd_id = gd_id.clone();
        let decided_by = decided_by.clone();
        let generated_policy = generated_policy.clone();
        let resolution_task = async move {
            // determinism-ok: async callback dispatch for governance decision resolution
            let system_tenant = TenantId::new("temper-system");
            if let Err(e) = state_c
                .dispatch_tenant_action(
                    &system_tenant,
                    "GovernanceDecision",
                    &gd_id,
                    "Approve",
                    serde_json::json!({
                        "decided_by": decided_by,
                        "scope": "narrow",
                        "generated_policy": generated_policy,
                    }),
                    &AgentContext::for_service("platform-dispatch"),
                )
                .await
            {
                tracing::error!(
                    error = %e, gd_id, "failed to dispatch GovernanceDecision.Approve"
                );
            }
        };
        tokio::spawn(resolution_task); // determinism-ok: async callback dispatch for governance decision resolution
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "id": id,
            "status": "approved",
            "generated_policy": generated_policy,
        })),
    )
        .into_response()
}

/// POST /api/tenants/{tenant}/decisions/{id}/deny — mark as denied.
#[instrument(skip_all, fields(tenant, id, otel.name = "POST /api/tenants/{tenant}/decisions/{id}/deny"))]
pub(crate) async fn handle_deny_decision(
    State(state): State<ServerState>,
    Path((_tenant, id)): Path<(String, String)>,
    auth: PolicyAuthed,
    _body: Option<axum::Json<serde_json::Value>>,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str().to_string();
    let principal_id = auth.security_context().principal.id.clone();

    // Read decision from the durable metadata backend.
    let mut decision: PendingDecision = {
        let Some(store) = state.metadata_store_for_tenant(&tenant).await else {
            tracing::error!("durable metadata backend not configured for deny decision");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "durable metadata backend not configured",
            )
                .into_response();
        };
        match store.get_pending_decision(&tenant, &id).await {
            Ok(Some(data_str)) => match serde_json::from_str::<PendingDecision>(&data_str) {
                Ok(d) if d.tenant == tenant => d,
                _ => {
                    tracing::warn!("decision not found for denial");
                    return (StatusCode::NOT_FOUND, "Decision not found").into_response();
                }
            },
            Ok(None) => {
                tracing::warn!("decision not found for denial");
                return (StatusCode::NOT_FOUND, "Decision not found").into_response();
            }
            Err(e) => {
                tracing::error!(error = %e, backend = store.backend_name(), "failed to load decision");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to load decision: {e}"),
                )
                    .into_response();
            }
        }
    };

    if decision.status != DecisionStatus::Pending {
        tracing::warn!(status = ?decision.status, "decision already resolved");
        return (
            StatusCode::CONFLICT,
            format!("Decision already resolved as {:?}", decision.status),
        )
            .into_response();
    }

    if let Some(resp) = decisions_access::reject_self_resolution(&principal_id, &decision) {
        return resp;
    }

    decision.status = DecisionStatus::Denied;
    decision.decided_by = Some(principal_id);
    decision.decided_at = Some(sim_now().to_rfc3339());
    let denied_decision = decision.clone();

    // Persist updated decision synchronously.
    if let Err(e) = state.persist_pending_decision(&denied_decision).await {
        tracing::warn!(error = %e, "failed to persist denied decision");
    }
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Decisions);

    // Dispatch GovernanceDecision.Deny — triggers DispatchCallback effect
    // which fails the waiting Session via the registered callback.
    if let Some(ref gd_id) = denied_decision.governance_decision_id {
        let state_c = state.clone();
        let gd_id = gd_id.clone();
        let decided_by_val = denied_decision
            .decided_by
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let resolution_task = async move {
            // determinism-ok: async callback dispatch for governance decision resolution
            let system_tenant = TenantId::new("temper-system");
            if let Err(e) = state_c
                .dispatch_tenant_action(
                    &system_tenant,
                    "GovernanceDecision",
                    &gd_id,
                    "Deny",
                    serde_json::json!({
                        "decided_by": decided_by_val,
                        "denial_reason": "Denied by human reviewer",
                    }),
                    &AgentContext::for_service("platform-dispatch"),
                )
                .await
            {
                tracing::error!(
                    error = %e, gd_id, "failed to dispatch GovernanceDecision.Deny"
                );
            }
        };
        tokio::spawn(resolution_task); // determinism-ok: async callback dispatch for governance decision resolution
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"id": id, "status": "denied"})),
    )
        .into_response()
}

/// GET /api/decisions — list decisions in the credential-bound tenant.
#[instrument(skip_all, fields(otel.name = "GET /api/decisions"))]
pub(crate) async fn handle_list_all_decisions(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(params): Query<DecisionListParams>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if let Err(status) = require_observe_auth(&state, authenticated, "manage_policies", "PolicySet")
    {
        return (status, "Authorization required").into_response();
    }
    let tenant = authenticated.tenant().as_str();
    let mut all_data = Vec::new();
    if let Some(store) = state.metadata_store_for_tenant(tenant).await {
        match store
            .query_decisions(tenant, params.status.as_deref())
            .await
        {
            Ok(data_strings) => all_data.extend(data_strings),
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), tenant, "failed to query decisions from metadata store");
            }
        }
    }
    if !all_data.is_empty() {
        return format_decision_list(all_data);
    }
    empty_decision_list()
}
