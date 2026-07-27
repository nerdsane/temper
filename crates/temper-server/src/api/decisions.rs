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
use temper_runtime::scheduler::sim_now;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use temper_runtime::tenant::TenantId;

use super::{PolicyAuthed, decisions_access, empty_decision_list, format_decision_list};
use crate::authz::require_observe_auth;
use crate::request_context::AgentContext;
use crate::state::{
    DecisionResolutionKind, DecisionResolutionPhase, DecisionStatus, PendingDecision, ServerState,
};

#[path = "decisions_approve.rs"]
mod approve;
#[path = "decisions_resolution.rs"]
mod resolution;
pub(crate) use approve::handle_approve_decision;

/// Query parameters for listing decisions.
#[derive(serde::Deserialize)]
pub(crate) struct DecisionListParams {
    /// Optional status filter: "pending", "approved", "denied", "expired".
    status: Option<String>,
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
    let access = match decisions_access::decision_list_access(&state, &headers, &tenant).await {
        Ok(access) => access,
        Err(resp) => return resp,
    };
    if let Some(store) = state.metadata_store_for_tenant(&tenant).await {
        match store
            .query_decisions(&tenant, params.status.as_deref())
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

/// POST /api/tenants/{tenant}/decisions/{id}/deny — mark as denied.
#[instrument(skip_all, fields(tenant, id, otel.name = "POST /api/tenants/{tenant}/decisions/{id}/deny"))]
pub(crate) async fn handle_deny_decision(
    State(state): State<ServerState>,
    Path((tenant, id)): Path<(String, String)>,
    _auth: PolicyAuthed,
    body: Option<axum::Json<serde_json::Value>>,
) -> impl IntoResponse {
    let decided_by = body
        .as_ref()
        .and_then(|b| b.get("decided_by"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let Some(store) = state.metadata_store_for_tenant(&tenant).await else {
        tracing::error!("durable metadata backend not configured for deny decision");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable metadata backend not configured",
        )
            .into_response();
    };
    let mut decision: PendingDecision = match store.get_pending_decision(&id).await {
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
    };

    if decision.status == DecisionStatus::Denied {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({"id": id, "status": "denied"})),
        )
            .into_response();
    }
    if decision.status != DecisionStatus::Pending {
        tracing::warn!(status = ?decision.status, "decision already resolved");
        return (
            StatusCode::CONFLICT,
            format!("Decision already resolved as {:?}", decision.status),
        )
            .into_response();
    }

    let decided_by_value = decided_by.unwrap_or_else(|| "unknown".to_string());
    let owner =
        resolution::resolution_owner(&decision, DecisionResolutionKind::Deny, &decided_by_value);
    decision =
        match resolution::claim_or_resume(&store, &decision, &owner, DecisionResolutionKind::Deny)
            .await
        {
            Ok(decision) => decision,
            Err(error) => return (StatusCode::CONFLICT, error).into_response(),
        };

    // Dispatch GovernanceDecision.Deny — triggers DispatchCallback effect
    // which fails the waiting Session via the registered callback.
    if decision.resolution_phase != Some(DecisionResolutionPhase::GovernanceDispatched)
        && let Some(ref gd_id) = decision.governance_decision_id
    {
        let mut context = AgentContext::for_service("platform-dispatch");
        context.idempotency_key = Some(format!("governance-denial:{tenant}:{id}"));
        let response = state
            .dispatch_tenant_action(
                &TenantId::new("temper-system"),
                "GovernanceDecision",
                gd_id,
                "Deny",
                serde_json::json!({
                    "decided_by": decided_by_value,
                    "denial_reason": "Denied by human reviewer",
                }),
                &context,
            )
            .await;
        match response {
            Ok(response)
                if response.success
                    && matches!(response.state.status.as_str(), "Denying" | "Denied") =>
            {
                let terminal = state
                    .get_tenant_entity_state(
                        &TenantId::new("temper-system"),
                        "GovernanceDecision",
                        gd_id,
                    )
                    .await;
                match terminal {
                    Ok(terminal) if terminal.state.status == "Denied" => {}
                    Ok(terminal) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            format!(
                                "GovernanceDecision effects completed without final Denied status: {:?}",
                                terminal.state.status
                            ),
                        )
                            .into_response();
                    }
                    Err(error) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            format!("failed to read finalized GovernanceDecision: {error}"),
                        )
                            .into_response();
                    }
                }
            }
            Ok(response) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    response
                        .error
                        .unwrap_or_else(|| "GovernanceDecision.Deny failed".to_string()),
                )
                    .into_response();
            }
            Err(error) => return (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
        }
    }
    if decision.resolution_phase != Some(DecisionResolutionPhase::GovernanceDispatched) {
        decision.resolution_phase = Some(DecisionResolutionPhase::GovernanceDispatched);
        if let Err(error) = resolution::persist_resolution_progress(&store, &decision, &owner).await
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
    }

    decision.status = DecisionStatus::Denied;
    decision.decided_by = Some(decided_by_value);
    decision.decided_at = Some(sim_now().to_rfc3339());
    if let Err(error) = resolution::complete_resolution(&store, &decision, &owner).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Decisions);

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
    _auth: PolicyAuthed,
) -> impl IntoResponse {
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
    let stores = state.collect_all_metadata_stores().await;
    let mut all_data = Vec::new();
    for store in &stores {
        match store.query_all_decisions(params.status.as_deref()).await {
            Ok(data_strings) => all_data.extend(data_strings),
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), "failed to query decisions from metadata store");
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
