//! Atomic boundary for approving pending authorization decisions.
//!
//! Policy generation, durable installation, GovernanceDecision resolution,
//! and PendingDecision status persistence are intentionally kept in one
//! decision-scoped path. The GovernanceDecision spec's post-commit hook only
//! verifies the preinstalled receipt; it does not generate or append policy.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_evolution::records::{Decision, DecisionRecord, RecordHeader, RecordType};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use super::PolicyAuthed;
use super::resolution::{
    claim_or_resume, complete_resolution, persist_resolution_progress, release_resolution,
    resolution_owner,
};
use crate::authz::{
    DecisionPolicyInstall, DecisionPolicyReceipt, install_decision_policy,
    rollback_created_decision_policy,
};
use crate::request_context::AgentContext;
use crate::state::{
    DecisionResolutionKind, DecisionResolutionPhase, DecisionStatus, PendingDecision, ServerState,
};
use crate::storage::MetadataStore;

/// Body for an approval request.
#[derive(serde::Deserialize)]
pub(crate) struct ApproveBody {
    /// Exact policy scope matrix approved by the reviewer.
    scope: temper_authz::PolicyScopeMatrix,
    /// Optional reviewer identity for audit attribution.
    decided_by: Option<String>,
}

async fn load_pending_decision(
    state: &ServerState,
    tenant: &str,
    id: &str,
) -> Result<PendingDecision, Response> {
    let Some(store) = state.metadata_store_for_tenant(tenant).await else {
        tracing::error!("durable metadata backend not configured for approve decision");
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "durable metadata backend not configured",
        )
            .into_response());
    };
    match store.get_pending_decision(id).await {
        Ok(Some(data)) => match serde_json::from_str::<PendingDecision>(&data) {
            Ok(decision) if decision.tenant == tenant => Ok(decision),
            _ => Err((StatusCode::NOT_FOUND, "Decision not found").into_response()),
        },
        Ok(None) => Err((StatusCode::NOT_FOUND, "Decision not found").into_response()),
        Err(error) => {
            tracing::error!(error = %error, backend = store.backend_name(), "failed to load decision");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load decision: {error}"),
            )
                .into_response())
        }
    }
}

fn approval_response(id: &str, generated_policy: &str) -> Response {
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

async fn dispatch_governance_approval(
    state: &ServerState,
    tenant: &str,
    pending_decision_id: &str,
    governance_decision_id: &str,
    receipt: &DecisionPolicyReceipt,
    decided_by: &str,
    generated_policy: &str,
) -> Result<(), String> {
    let encoded_receipt = receipt.encode()?;
    let mut context = AgentContext::for_service("platform-dispatch");
    context.idempotency_key = Some(format!(
        "governance-approval:{tenant}:{pending_decision_id}"
    ));
    let response = state
        .dispatch_tenant_action(
            &TenantId::new("temper-system"),
            "GovernanceDecision",
            governance_decision_id,
            "Approve",
            serde_json::json!({
                "decided_by": decided_by,
                "scope": encoded_receipt,
                "generated_policy": generated_policy,
            }),
            &context,
        )
        .await
        .map_err(|error| format!("GovernanceDecision.Approve dispatch failed: {error}"))?;
    if !response.success {
        return Err(response
            .error
            .unwrap_or_else(|| "GovernanceDecision.Approve effects failed".to_string()));
    }
    if !matches!(response.state.status.as_str(), "Approving" | "Approved") {
        return Err(format!(
            "GovernanceDecision.Approve returned unexpected status {:?}",
            response.state.status
        ));
    }
    let terminal = state
        .get_tenant_entity_state(
            &TenantId::new("temper-system"),
            "GovernanceDecision",
            governance_decision_id,
        )
        .await
        .map_err(|error| format!("failed to read finalized GovernanceDecision: {error}"))?;
    if terminal.state.status != "Approved" {
        return Err(format!(
            "GovernanceDecision effects completed without final Approved status: {:?}",
            terminal.state.status
        ));
    }
    Ok(())
}

async fn persist_approval_audit(
    state: &ServerState,
    decision: &PendingDecision,
    scope: &temper_authz::PolicyScopeMatrix,
    generated_policy: &str,
) {
    let header = RecordHeader::new(RecordType::Decision, "human:approval");
    let header = match decision.evolution_record_id.as_ref() {
        Some(id) => header.derived_from(id.clone()),
        None => header,
    };
    let record = DecisionRecord {
        header,
        decision: Decision::Approved,
        decided_by: decision
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
    let Some(store) = state.platform_metadata_store() else {
        return;
    };
    let data_json = match serde_json::to_string(&record) {
        Ok(data_json) => data_json,
        Err(error) => {
            tracing::warn!(error = %error, "failed to serialize approval D-Record");
            return;
        }
    };
    if let Err(error) = store
        .insert_evolution_record(
            &record.header.id,
            "Decision",
            &format!("{:?}", record.header.status),
            &record.header.created_by,
            record.header.derived_from.as_deref(),
            &data_json,
        )
        .await
    {
        tracing::warn!(error = %error, backend = store.backend_name(), "failed to persist D-Record");
    }
}

#[derive(Clone, Copy)]
struct CreatedPolicyRollback<'a> {
    tenant: &'a str,
    policy_id: &'a str,
    install: DecisionPolicyInstall,
}

async fn rollback_before_governance_dispatch(
    state: &ServerState,
    store: &Arc<dyn MetadataStore>,
    pending: &PendingDecision,
    owner: &str,
    rollback: CreatedPolicyRollback<'_>,
    dispatch_error: String,
) -> Response {
    if let DecisionPolicyInstall::Created {
        publication_version,
    } = rollback.install
        && let Err(rollback_error) = rollback_created_decision_policy(
            state,
            rollback.tenant,
            rollback.policy_id,
            publication_version,
        )
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "{dispatch_error}; newly-created policy rollback also failed: {rollback_error}"
            ),
        )
            .into_response();
    }
    if matches!(rollback.install, DecisionPolicyInstall::Created { .. })
        && let Err(release_error) = release_resolution(store, pending, owner).await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{dispatch_error}; decision ownership release failed: {release_error}"),
        )
            .into_response();
    }
    (StatusCode::BAD_GATEWAY, dispatch_error).into_response()
}

/// POST /api/tenants/{tenant}/decisions/{id}/approve — approve with scope.
#[instrument(skip_all, fields(tenant, id, otel.name = "POST /api/tenants/{tenant}/decisions/{id}/approve"))]
pub(crate) async fn handle_approve_decision(
    State(state): State<ServerState>,
    Path((tenant, id)): Path<(String, String)>,
    _auth: PolicyAuthed,
    axum::Json(body): axum::Json<ApproveBody>,
) -> Response {
    if let Err(error) = temper_authz::validate_policy_scope_matrix(&body.scope) {
        return (
            StatusCode::BAD_REQUEST,
            format!("Invalid policy scope matrix: {error}"),
        )
            .into_response();
    }

    let mut decision = match load_pending_decision(&state, &tenant, &id).await {
        Ok(decision) => decision,
        Err(response) => return response,
    };
    let principal_kind = match decision
        .principal_kind
        .as_deref()
        .filter(|kind| !kind.trim().is_empty())
    {
        Some(principal_kind) => principal_kind.to_string(),
        None => {
            return (
                StatusCode::CONFLICT,
                "Decision is missing its authenticated principal kind",
            )
                .into_response();
        }
    };
    let generated_policy = match decision.generate_policy_from_matrix(&body.scope) {
        Ok(policy) => policy,
        Err(error) => {
            tracing::error!(error = %error, "failed to generate policy from scope matrix");
            return (
                StatusCode::BAD_REQUEST,
                format!("Failed to generate policy: {error}"),
            )
                .into_response();
        }
    };
    let policy_id = format!("decision:{id}");
    let decided_by = body.decided_by.as_deref().unwrap_or("unknown");

    // A retry of the exact approved request is idempotent. It repairs a cold
    // in-memory engine from the immutable durable row, but never redelivers the
    // governance callback or creates another audit record.
    if decision.status == DecisionStatus::Approved {
        let scope_matches = decision
            .approved_scope
            .as_ref()
            .is_some_and(|scope| scope == &body.scope);
        if !scope_matches || decision.generated_policy.as_deref() != Some(&generated_policy) {
            return (
                StatusCode::CONFLICT,
                "Decision was already approved with different policy content",
            )
                .into_response();
        }
        if let Err(error) =
            install_decision_policy(&state, &tenant, &policy_id, &generated_policy, decided_by)
                .await
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Approved policy recovery failed: {error}"),
            )
                .into_response();
        }
        return approval_response(&id, &generated_policy);
    }
    if decision.status != DecisionStatus::Pending {
        return (
            StatusCode::CONFLICT,
            format!("Decision already resolved as {:?}", decision.status),
        )
            .into_response();
    }

    let receipt = decision
        .governance_decision_id
        .as_ref()
        .map(|gd_id| DecisionPolicyReceipt {
            pending_decision_id: id.clone(),
            governance_decision_id: gd_id.clone(),
            principal_kind: principal_kind.clone(),
            scope_matrix: body.scope.clone(),
        });
    // Encode before mutating durable policy state. Serialization failure must
    // leave the PendingDecision and active authorization set untouched.
    if let Some(receipt) = receipt.as_ref()
        && let Err(error) = receipt.encode()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }

    let Some(store) = state.metadata_store_for_tenant(&tenant).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable metadata backend not configured",
        )
            .into_response();
    };
    let pending_before_claim = decision.clone();
    let newly_claimed = decision.resolution_owner.is_none();
    let request_binding = format!("{decided_by}\0{generated_policy}");
    let owner = resolution_owner(&decision, DecisionResolutionKind::Approve, &request_binding);
    decision =
        match claim_or_resume(&store, &decision, &owner, DecisionResolutionKind::Approve).await {
            Ok(decision) => decision,
            Err(error) => return (StatusCode::CONFLICT, error).into_response(),
        };
    let governance_already_dispatched =
        decision.resolution_phase == Some(DecisionResolutionPhase::GovernanceDispatched);

    let install = match install_decision_policy(
        &state,
        &tenant,
        &policy_id,
        &generated_policy,
        decided_by,
    )
    .await
    {
        Ok(install) => install,
        Err(error) => {
            if newly_claimed
                && let Err(release_error) =
                    release_resolution(&store, &pending_before_claim, &owner).await
            {
                return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "Failed to install approved policy: {error}; decision ownership release failed: {release_error}"
                        ),
                    )
                        .into_response();
            }
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Failed to install approved policy: {error}"),
            )
                .into_response();
        }
    };
    let publication_version = match install {
        DecisionPolicyInstall::Created {
            publication_version,
        }
        | DecisionPolicyInstall::AlreadyPresent {
            publication_version,
        } => publication_version,
    };
    if matches!(install, DecisionPolicyInstall::Created { .. })
        && decision
            .resolution_policy_version
            .is_some_and(|version| version != publication_version)
    {
        return (
            StatusCode::CONFLICT,
            "approval owner is bound to a different policy publication version",
        )
            .into_response();
    }
    if !governance_already_dispatched
        && decision.resolution_phase != Some(DecisionResolutionPhase::PolicyPublished)
    {
        decision
            .resolution_policy_version
            .get_or_insert(publication_version);
        decision.resolution_phase = Some(DecisionResolutionPhase::PolicyPublished);
        if let Err(error) = persist_resolution_progress(&store, &decision, &owner).await {
            return rollback_before_governance_dispatch(
                &state,
                &store,
                &pending_before_claim,
                &owner,
                CreatedPolicyRollback {
                    tenant: &tenant,
                    policy_id: &policy_id,
                    install,
                },
                format!("failed to persist policy publication receipt: {error}"),
            )
            .await;
        }
    }

    if !governance_already_dispatched
        && let (Some(governance_id), Some(receipt)) =
            (decision.governance_decision_id.as_deref(), receipt.as_ref())
        && let Err(error) = dispatch_governance_approval(
            &state,
            &tenant,
            &id,
            governance_id,
            receipt,
            decided_by,
            &generated_policy,
        )
        .await
    {
        // The GovernanceDecision transition may already be durably Approving
        // and its composite effect may have delivered the callback before a
        // later finalization fault. Retain the exact owner and policy so only
        // this request can retry the same idempotent protocol; rollback here
        // could admit a contradictory Deny owner.
        return (StatusCode::BAD_GATEWAY, error).into_response();
    }
    if !governance_already_dispatched {
        decision.resolution_phase = Some(DecisionResolutionPhase::GovernanceDispatched);
        if let Err(error) = persist_resolution_progress(&store, &decision, &owner).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to persist governance dispatch receipt: {error}"),
            )
                .into_response();
        }
    }

    // Only now can the REST-facing decision become Approved: durable policy
    // persistence and activation succeeded, and the linked governance effects
    // completed successfully.
    decision.status = DecisionStatus::Approved;
    decision.approved_scope = Some(body.scope.clone());
    decision.generated_policy = Some(generated_policy.clone());
    decision.decided_by = body.decided_by;
    decision.decided_at = Some(sim_now().to_rfc3339());
    if let Err(error) = complete_resolution(&store, &decision, &owner).await {
        // Do not roll back here: GovernanceDecision may already be terminal and
        // its callback delivered. Returning an error allows an exact retry to
        // converge the PendingDecision using the same durable policy/dispatch id.
        tracing::error!(id, error = %error, "failed to persist approved decision");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to complete approved decision: {error}"),
        )
            .into_response();
    }
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Decisions);
    persist_approval_audit(&state, &decision, &body.scope, &generated_policy).await;
    approval_response(&id, &generated_policy)
}

#[cfg(test)]
#[path = "decisions_approve_test.rs"]
mod tests;
