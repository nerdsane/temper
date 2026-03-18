//! Decision management API endpoints.
//!
//! Handles listing, approving, and denying evolution decisions, plus SSE
//! streaming for real-time decision notifications (both per-tenant and
//! cross-tenant).

use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_evolution::records::{Decision, DecisionRecord, RecordHeader, RecordType};
use temper_runtime::scheduler::sim_now;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use super::{empty_decision_list, format_decision_list, require_policy_auth};
use crate::authz::{persist_and_activate_policy, require_observe_auth};
use crate::state::{DecisionStatus, PendingDecision, ServerState};

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
    /// Optional: who approved.
    decided_by: Option<String>,
}

/// GET /api/tenants/{tenant}/decisions — list decisions with optional status filter.
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/decisions"))]
pub(crate) async fn handle_list_decisions(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    Query(params): Query<DecisionListParams>,
) -> impl IntoResponse {
    if let Some(resp) = require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }
    if let Some(turso) = state.persistent_store_for_tenant(&tenant).await {
        match turso
            .query_decisions(&tenant, params.status.as_deref())
            .await
        {
            Ok(data_strings) => return format_decision_list(data_strings),
            Err(e) => {
                tracing::warn!(error = %e, "failed to query decisions from Turso");
            }
        }
    }
    empty_decision_list()
}

/// POST /api/tenants/{tenant}/decisions/{id}/approve — approve with scope.
#[instrument(skip_all, fields(tenant, id, otel.name = "POST /api/tenants/{tenant}/decisions/{id}/approve"))]
pub(crate) async fn handle_approve_decision(
    State(state): State<ServerState>,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ApproveBody>,
) -> impl IntoResponse {
    if let Some(resp) = require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }

    let scope = body.scope;
    if let Err(e) = temper_authz::validate_policy_scope_matrix(&scope) {
        return (
            StatusCode::BAD_REQUEST,
            format!("Invalid policy scope matrix: {e}"),
        )
            .into_response();
    }

    // Read decision from Turso (single source of truth).
    let mut decision: PendingDecision = {
        let Some(turso) = state.persistent_store_for_tenant(&tenant).await else {
            tracing::error!("Turso backend not configured for approve decision");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Turso backend not configured",
            )
                .into_response();
        };
        match turso.get_pending_decision(&id).await {
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
                tracing::error!(error = %e, "failed to load decision from Turso");
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

    let generated_policy = decision.generate_policy_from_matrix(&scope);
    let evolution_record_id = decision.evolution_record_id.clone();

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
    if let Err(resp) = super::validate_and_reload_policies(&state, &tenant, &prospective) {
        return resp;
    }

    // Persist the individual policy to the granular `policies` table.
    let decided_by_ref = body.decided_by.as_deref().unwrap_or("unknown");
    persist_and_activate_policy(
        &state,
        &tenant,
        &format!("decision:{id}"),
        &generated_policy,
        decided_by_ref,
    )
    .await;

    // Update in-memory map to reflect the new policy.
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.clone(), prospective);
    }

    // Mark decision approved only after policy reload succeeds.
    decision.status = DecisionStatus::Approved;
    decision.approved_scope = Some(scope.clone());
    decision.generated_policy = Some(generated_policy.clone());
    decision.decided_by = body.decided_by.clone();
    decision.decided_at = Some(sim_now().to_rfc3339());
    let approved_decision = decision.clone();

    // Persist updated decision to Turso synchronously.
    if let Err(e) = state.persist_pending_decision(&approved_decision).await {
        tracing::warn!(id = %id, error = %e, "failed to persist approved decision");
    }

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
        decided_by: body
            .decided_by
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        rationale: format!(
            "Approved with scope: {:?}. Policy: {}",
            scope, generated_policy
        ),
        verification_results: None,
        implementation: None,
    };
    // Persist D-Record to Turso (evolution records stay on platform DB).
    if let Some(turso) = state.platform_persistent_store() {
        let data_json = serde_json::to_string(&d_record).unwrap_or_default();
        if let Err(e) = turso
            .insert_evolution_record(
                &d_record.header.id,
                "Decision",
                &format!("{:?}", d_record.header.status),
                &d_record.header.created_by,
                d_record.header.derived_from.as_deref(),
                &data_json,
            )
            .await
        {
            tracing::warn!(error = %e, "failed to persist D-Record to Turso");
        }
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
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<axum::Json<serde_json::Value>>,
) -> impl IntoResponse {
    if let Some(resp) = require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }

    let decided_by = body
        .as_ref()
        .and_then(|b| b.get("decided_by"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // Read decision from Turso (single source of truth).
    let mut decision: PendingDecision = {
        let Some(turso) = state.persistent_store_for_tenant(&tenant).await else {
            tracing::error!("Turso backend not configured for deny decision");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Turso backend not configured",
            )
                .into_response();
        };
        match turso.get_pending_decision(&id).await {
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
                tracing::error!(error = %e, "failed to load decision from Turso");
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

    decision.status = DecisionStatus::Denied;
    decision.decided_by = decided_by;
    decision.decided_at = Some(sim_now().to_rfc3339());
    let denied_decision = decision.clone();

    // Persist updated decision to Turso synchronously.
    if let Err(e) = state.persist_pending_decision(&denied_decision).await {
        tracing::warn!(error = %e, "failed to persist denied decision");
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"id": id, "status": "denied"})),
    )
        .into_response()
}

/// GET /api/tenants/{tenant}/decisions/stream — SSE for new pending decisions.
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/decisions/stream"))]
pub(crate) async fn handle_decision_stream(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(resp) = require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(pd) => {
                if pd.tenant != tenant {
                    return None;
                }
                let data = serde_json::to_string(&pd).unwrap_or_default();
                Some(Ok::<Event, Infallible>(
                    Event::default().event("pending_decision").data(data),
                ))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// GET /api/decisions — list all decisions across all tenants.
///
/// Requires admin-level authorization for cross-tenant visibility.
#[instrument(skip_all, fields(otel.name = "GET /api/decisions"))]
pub(crate) async fn handle_list_all_decisions(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<DecisionListParams>,
) -> impl IntoResponse {
    if let Err(status) = require_observe_auth(&state, &headers, "manage_policies", "PolicySet") {
        return (status, "Authorization required for cross-tenant access").into_response();
    }
    // Fan-out across all tenant stores to aggregate decisions.
    let stores = state.collect_all_turso_stores().await;
    let mut all_data = Vec::new();
    for turso in &stores {
        match turso.query_all_decisions(params.status.as_deref()).await {
            Ok(data_strings) => all_data.extend(data_strings),
            Err(e) => {
                tracing::warn!(error = %e, "failed to query decisions from a Turso store");
            }
        }
    }
    if !all_data.is_empty() {
        return format_decision_list(all_data);
    }
    empty_decision_list()
}

/// GET /api/decisions/stream — SSE for all pending decisions across all tenants.
///
/// Requires admin-level authorization for cross-tenant visibility.
#[instrument(skip_all, fields(otel.name = "GET /api/decisions/stream"))]
pub(crate) async fn handle_all_decisions_stream(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(status) = require_observe_auth(&state, &headers, "manage_policies", "PolicySet") {
        return (status, "Authorization required for cross-tenant access").into_response();
    }
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
        Ok(pd) => {
            let data = serde_json::to_string(&pd).unwrap_or_default();
            Some(Ok::<Event, Infallible>(
                Event::default().event("pending_decision").data(data),
            ))
        }
        Err(_) => None,
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// GET /api/agents/{agent_id}/stream — SSE for agent progress events.
///
/// Requires admin-level authorization.
#[instrument(skip_all, fields(agent_id, otel.name = "GET /api/agents/{agent_id}/stream"))]
pub(crate) async fn handle_agent_progress_stream(
    State(state): State<ServerState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(status) = require_observe_auth(&state, &headers, "read_agents", "AgentAudit") {
        return (status, "Authorization required").into_response();
    }
    let rx = state.agent_progress_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(event) => {
                if event.agent_id != agent_id {
                    return None;
                }
                let data = serde_json::to_string(&event).unwrap_or_default();
                Some(Ok::<Event, Infallible>(
                    Event::default().event(&event.kind).data(data),
                ))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
