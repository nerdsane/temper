//! Management API routes (mutations).
//!
//! These endpoints handle spec loading, WASM module management, and evolution
//! decisions.  They are separated from the read-only `/observe` router so that
//! observe stays purely observational.

mod authorize;
mod decisions;
mod decisions_access;
mod decisions_get;
mod files;
mod policies;
mod repl;
mod secrets;

use axum::Router;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, patch, post, put};
use temper_authz::PrincipalKind;

use crate::authz::{DenialInput, record_authz_denial, security_context_from_headers};
use crate::response::service_unavailable_response;
use crate::state::ServerState;

/// Build the management API router (mounted at /api).
///
/// Route structure:
/// - POST   /api/specs/load-dir                        -> load specs from directory
/// - POST   /api/specs/load-inline                     -> load specs from inline payload
/// - POST   /api/specs/validate-ioa                    -> validate IOA source without loading it
/// - POST   /api/wasm/modules/{module_name}            -> upload WASM module
/// - DELETE /api/wasm/modules/{module_name}             -> delete WASM module
/// - POST   /api/evolution/records/{id}/decide          -> developer decision on record
/// - POST   /api/evolution/trajectories/unmet           -> report unmet user intent
/// - POST   /api/evolution/sentinel/check               -> trigger sentinel health check
/// - POST   /api/evolution/analyze                      -> run IntentDiscovery loop
/// - POST   /api/evolution/materialize                  -> persist O/P/A/I + PM issues
/// - POST   /api/files/read-text-batch                  -> batch current-file text reads via projections + blobs
/// - POST   /api/files/read-version-text-batch          -> batch immutable file-version text reads
/// - POST   /api/files/publish-artifact                 -> promote a governed file to a public immutable artifact
pub fn build_api_router() -> Router<ServerState> {
    Router::new()
        .route(
            "/specs/load-dir",
            post(crate::observe::specs::handle_load_dir),
        )
        .route(
            "/specs/load-inline",
            post(crate::observe::specs::handle_load_inline),
        )
        .route(
            "/specs/validate-ioa",
            post(crate::observe::specs::handle_validate_ioa),
        )
        .route(
            "/wasm/modules/{module_name}",
            post(crate::observe::wasm::handle_upload_wasm_module)
                .delete(crate::observe::wasm::handle_delete_wasm_module),
        )
        .route(
            "/evolution/records/{id}/decide",
            post(crate::observe::evolution::handle_decide),
        )
        .route(
            "/evolution/trajectories/unmet",
            post(crate::observe::evolution::handle_unmet_intent),
        )
        .route(
            "/evolution/sentinel/check",
            post(crate::observe::evolution::handle_sentinel_check),
        )
        .route(
            "/evolution/analyze",
            post(crate::observe::evolution::handle_evolution_analyze),
        )
        .route(
            "/evolution/materialize",
            post(crate::observe::evolution::handle_evolution_materialize),
        )
        .route(
            "/files/read-text-batch",
            post(files::handle_read_text_batch),
        )
        .route(
            "/files/read-version-text-batch",
            post(files::handle_read_version_text_batch),
        )
        .route(
            "/files/publish-artifact",
            post(files::handle_publish_artifact),
        )
        // OTS trajectory endpoints (full agent execution traces for GEPA)
        .route(
            "/ots/trajectories",
            post(crate::observe::evolution::handle_post_ots_trajectory)
                .get(crate::observe::evolution::handle_get_ots_trajectories),
        )
        .route(
            "/tenants/{tenant}/secrets/{key_name}",
            put(secrets::handle_put_secret).delete(secrets::handle_delete_secret),
        )
        .route(
            "/tenants/{tenant}/secrets",
            get(secrets::handle_list_secrets),
        )
        // Policy CRUD
        .route(
            "/tenants/{tenant}/policies",
            get(policies::handle_get_policies).put(policies::handle_put_policies),
        )
        .route(
            "/tenants/{tenant}/policies/rules",
            post(policies::handle_add_policy_rule),
        )
        .route(
            "/tenants/{tenant}/policies/list",
            get(policies::handle_list_policies),
        )
        .route(
            "/tenants/{tenant}/policies/create",
            post(policies::handle_create_policy),
        )
        .route(
            "/tenants/{tenant}/policies/entry/{policy_id}",
            patch(policies::handle_patch_policy).delete(policies::handle_delete_policy_entry),
        )
        .route(
            "/tenants/{tenant}/policies/suggestions",
            get(handle_policy_suggestions),
        )
        // Cross-tenant policy listing
        .route("/policies", get(policies::handle_list_all_policies))
        // Decision approve/deny (Phase 4)
        .route(
            "/tenants/{tenant}/decisions",
            get(decisions::handle_list_decisions),
        )
        .route(
            "/tenants/{tenant}/decisions/stream",
            get(decisions::handle_decision_stream),
        )
        .route(
            "/tenants/{tenant}/decisions/{id}",
            get(decisions_get::handle_get_decision),
        )
        .route(
            "/tenants/{tenant}/decisions/{id}/approve",
            post(decisions::handle_approve_decision),
        )
        .route(
            "/tenants/{tenant}/decisions/{id}/deny",
            post(decisions::handle_deny_decision),
        )
        // REPL endpoint (Monty sandbox over HTTP)
        .route("/repl", post(repl::handle_repl))
        // Agent authorization + audit endpoints
        .route("/authorize", post(authorize::handle_authorize))
        .route("/audit", post(authorize::handle_audit))
        // Cross-tenant decision endpoints
        .route("/decisions", get(decisions::handle_list_all_decisions))
        .route(
            "/decisions/stream",
            get(decisions::handle_all_decisions_stream),
        )
        // Agent progress SSE endpoint
        .route(
            "/agents/{agent_id}/stream",
            get(decisions::handle_agent_progress_stream),
        )
}

/// Authorize a policy management request against Cedar policies.
///
/// Returns `Some(response)` if authorization is denied, `None` if allowed.
/// Admin principals always bypass Cedar for policy management.
pub(crate) async fn require_policy_auth(
    state: &ServerState,
    headers: &HeaderMap,
    tenant: &str,
) -> Option<axum::response::Response> {
    let security_ctx = security_context_from_headers(headers, None, None, None);
    if matches!(security_ctx.principal.kind, PrincipalKind::Admin) {
        // Admin principals (e.g. Observe UI) always bypass Cedar for policy
        // management. Without this, approving the first policy would lock out
        // the admin from managing subsequent decisions.
        return None;
    }
    if let Err(denial) = state.authorize_with_context(
        &security_ctx,
        "manage_policies",
        "PolicySet",
        &std::collections::BTreeMap::new(),
        tenant,
    ) {
        let reason = denial.to_string();
        let pd = record_authz_denial(
            state,
            DenialInput {
                tenant,
                security_ctx: &security_ctx,
                agent_id_override: None,
                action: "manage_policies",
                resource_type: "PolicySet",
                resource_id: tenant,
                resource_attrs: serde_json::json!({"tenant": tenant}),
                reason: &reason,
                module_name: None,
                from_status: None,
            },
        )
        .await;
        return Some(
            (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "error": {
                        "code": "AuthorizationDenied",
                        "message": format!("{reason} Decision {}", pd.id),
                    }
                })),
            )
                .into_response(),
        );
    }
    None
}

/// Cedar policy-management gate as an axum extractor.
///
/// Runs [`require_policy_auth`] against the `{tenant}` path parameter before
/// the handler body executes, rejecting with the exact response the helper
/// produces (403 + `AuthorizationDenied` JSON including the decision id).
/// The tenant is read from the request parts by name, so handlers keep their
/// own `Path<String>` / `Path<(String, String)>` extractors untouched.
pub(crate) struct PolicyAuthed;

impl FromRequestParts<ServerState> for PolicyAuthed {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        let Path(params) =
            Path::<std::collections::BTreeMap<String, String>>::from_request_parts(parts, state)
                .await
                .map_err(IntoResponse::into_response)?;
        let Some(tenant) = params.get("tenant") else {
            // Fail closed: every route using PolicyAuthed must declare a
            // {tenant} path parameter; reaching this branch is a routing bug.
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        };
        match require_policy_auth(state, &parts.headers, tenant).await {
            Some(resp) => Err(resp),
            None => Ok(Self),
        }
    }
}

/// GET /api/tenants/{tenant}/policies/suggestions — suggested policies from denial patterns.
async fn handle_policy_suggestions(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _auth: PolicyAuthed,
) -> impl IntoResponse {
    let suggestions = if let Some(store) = state.metadata_store_for_tenant(&tenant).await {
        match store.load_policy_denial_patterns(&tenant).await {
            Ok(rows) if !rows.is_empty() => {
                let mut engine = crate::state::policy_suggestions::PolicySuggestionEngine::new();
                for row in rows {
                    let distinct_resource_ids =
                        serde_json::from_str::<Vec<String>>(&row.distinct_resource_ids_json)
                            .unwrap_or_default();
                    engine.record_denial_snapshot(
                        crate::state::policy_suggestions::DenialSnapshot {
                            agent_type: row.agent_type.as_deref(),
                            action: &row.action,
                            resource_type: &row.resource_type,
                            count: row.count.max(0) as usize,
                            first_seen: &row.first_seen,
                            last_seen: &row.last_seen,
                            distinct_resource_ids,
                        },
                    );
                }
                engine.suggestions()
            }
            Ok(_) => match state.suggestion_engine.read() {
                Ok(engine) => engine.suggestions(),
                Err(_) => vec![],
            },
            Err(e) => {
                tracing::warn!(error = %e, tenant, backend = store.backend_name(), "failed to load persisted policy suggestions");
                match state.suggestion_engine.read() {
                    Ok(engine) => engine.suggestions(),
                    Err(_) => vec![],
                }
            }
        }
    } else {
        match state.suggestion_engine.read() {
            Ok(engine) => engine.suggestions(),
            Err(_) => vec![],
        }
    };
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "suggestions": suggestions })),
    )
        .into_response()
}

/// Validate and reload combined Cedar policies for a tenant mutation.
///
/// Builds a combined policy text from all tenants, substituting `new_tenant_text`
/// for the given tenant. Returns `Ok(())` on success, or an error response on
/// validation failure.
#[allow(clippy::result_large_err)]
pub(crate) fn validate_and_reload_policies(
    state: &ServerState,
    tenant: &str,
    new_tenant_text: &str,
) -> Result<(), axum::response::Response> {
    // Validate and reload only this tenant's policy set (per-tenant isolation).
    if let Err(e) = state.authz.reload_tenant_policies(tenant, new_tenant_text) {
        tracing::warn!(error = %e, "policy validation failed");
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Policy validation failed: {e}"),
        )
            .into_response());
    }
    Ok(())
}

/// Format decision query results into a JSON response with counts.
pub(crate) fn format_decision_list(data_strings: Vec<String>) -> axum::response::Response {
    let entries: Vec<serde_json::Value> = match data_strings
        .iter()
        .map(|data| serde_json::from_str(data))
        .collect::<Result<_, _>>()
    {
        Ok(entries) => entries,
        Err(error) => return decision_data_unavailable_response(error),
    };
    let pending_count = entries
        .iter()
        .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("pending"))
        .count();
    let approved_count = entries
        .iter()
        .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("approved"))
        .count();
    let denied_count = entries
        .iter()
        .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("denied"))
        .count();
    let total = entries.len();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "decisions": entries,
            "total": total,
            "pending_count": pending_count,
            "approved_count": approved_count,
            "denied_count": denied_count,
        })),
    )
        .into_response()
}

fn decision_data_unavailable_response(error: serde_json::Error) -> axum::response::Response {
    service_unavailable_response(
        "DecisionDataUnavailable",
        "Decision data is temporarily unavailable",
        "list decisions",
        error,
    )
}

/// Empty decision list response (used when no store is available).
pub(crate) fn empty_decision_list() -> axum::response::Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "decisions": [],
            "total": 0,
            "pending_count": 0,
            "approved_count": 0,
            "denied_count": 0,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn corrupt_durable_decision_returns_sanitized_service_unavailable() {
        const SECRET_SENTINEL: &str = "postgres://admin:secret@internal-db";
        let response = format_decision_list(vec![format!("{{{SECRET_SENTINEL}")]);

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("response body");
        let body = String::from_utf8(body.to_vec()).expect("utf-8 response");
        assert!(body.contains("DecisionDataUnavailable"));
        assert!(!body.contains(SECRET_SENTINEL));
    }
}
