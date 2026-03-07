//! GET /observe/verification-status -- all entity verification statuses.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Serialize;
use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_observe_auth};
use crate::registry::VerificationStatus;
use crate::state::ServerState;

/// Response shape for GET /observe/verification-status.
#[derive(Serialize)]
pub(in crate::observe) struct AllVerificationStatus {
    /// Aggregate counts.
    pending: usize,
    running: usize,
    passed: usize,
    failed: usize,
    partial: usize,
    /// Per-entity details.
    entities: Vec<EntityVerificationStatusResponse>,
}

/// Per-entity verification status in the response.
#[derive(Serialize)]
struct EntityVerificationStatusResponse {
    tenant: String,
    entity_type: String,
    status: String,
    levels: Option<Vec<serde_json::Value>>,
    verified_at: Option<String>,
}

/// GET /observe/verification-status -- all entity verification statuses.
#[instrument(skip_all, fields(otel.name = "GET /observe/verification-status"))]
pub(crate) async fn handle_verification_status(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<AllVerificationStatus>, StatusCode> {
    require_observe_auth(&state, &headers, "read_verification", "Spec")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let mut pending = 0usize;
    let mut running = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut partial = 0usize;
    let mut entities = Vec::new();

    for tenant_id in registry.tenant_ids() {
        if let Some(ref scope) = tenant_scope
            && tenant_id != scope
        {
            continue;
        }
        if let Some(statuses) = registry.verification_statuses(tenant_id) {
            for (entity_type, status) in statuses {
                let (status_str, levels, verified_at) = match status {
                    VerificationStatus::Pending => {
                        pending += 1;
                        ("pending".to_string(), None, None)
                    }
                    VerificationStatus::Running => {
                        running += 1;
                        ("running".to_string(), None, None)
                    }
                    VerificationStatus::Completed(result) | VerificationStatus::Restored(result) => {
                        let passed_count = result.levels.iter().filter(|l| l.passed).count();
                        let s = if result.all_passed {
                            passed += 1;
                            "passed"
                        } else if passed_count == 0 {
                            failed += 1;
                            "failed"
                        } else {
                            partial += 1;
                            "partial"
                        };
                        let lvls: Vec<serde_json::Value> = result
                            .levels
                            .iter()
                            .map(|l| {
                                let mut obj = serde_json::json!({
                                    "level": l.level,
                                    "passed": l.passed,
                                    "summary": l.summary,
                                });
                                if let Some(details) = &l.details {
                                    obj["details"] =
                                        serde_json::to_value(details).unwrap_or_default();
                                }
                                obj
                            })
                            .collect();
                        (s.to_string(), Some(lvls), Some(result.verified_at.clone()))
                    }
                };

                entities.push(EntityVerificationStatusResponse {
                    tenant: tenant_id.as_str().to_string(),
                    entity_type: entity_type.clone(),
                    status: status_str,
                    levels,
                    verified_at,
                });
            }
        }
    }

    Ok(Json(AllVerificationStatus {
        pending,
        running,
        passed,
        failed,
        partial,
        entities,
    }))
}
