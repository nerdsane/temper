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
mod reactions;
mod repl;
mod secrets;
mod spec_pin;
mod trajectory_analysis;

use axum::Router;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use axum::routing::{get, patch, post, put};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::tenant::TenantId;

use crate::authz::{
    DenialInput, record_authz_denial, require_authenticated_context, require_tenant_match,
};
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
/// - GET    /api/ots/trajectories/{id}/atif             -> export an OTS trajectory as ATIF v1.7
/// - POST   /api/conformance/check                      -> check a session against its actor spec
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
            "/ots/trajectories/{trajectory_id}/atif",
            get(trajectory_analysis::handle_get_ots_trajectory_atif),
        )
        // Deterministic conformance checking of a recorded run against its spec
        .route(
            "/conformance/check",
            post(trajectory_analysis::handle_conformance_check),
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
        .route(
            "/reactions/{delivery_id}/retry",
            post(reactions::handle_retry_reaction),
        )
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
pub(crate) async fn require_policy_auth(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
) -> Option<axum::response::Response> {
    let security_ctx = authenticated.security_context();
    let tenant = authenticated.tenant().as_str();
    let resource_attrs = std::collections::BTreeMap::from([
        (
            "id".to_string(),
            serde_json::Value::String(tenant.to_string()),
        ),
        (
            "tenant".to_string(),
            serde_json::Value::String(tenant.to_string()),
        ),
    ]);
    if let Err(denial) = state.authorize_with_context(
        security_ctx,
        "manage_policies",
        "PolicySet",
        &resource_attrs,
        tenant,
    ) {
        let reason = denial.to_string();
        let pd = record_authz_denial(
            state,
            DenialInput {
                tenant,
                security_ctx,
                agent_id_override: None,
                action: "manage_policies",
                resource_type: "PolicySet",
                resource_id: tenant,
                resource_attrs: serde_json::json!({"tenant": tenant}),
                reason: &reason,
                module_name: None,
                from_status: None,
                intent: authenticated.intent().map(str::to_string),
                session_id: authenticated.session_id().map(str::to_string),
                // Management-plane denial, not a spec-governed dispatch.
                spec_governed: Some(false),
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
pub(crate) struct PolicyAuthed(AuthenticatedRequestContext);

impl PolicyAuthed {
    /// Credential-bound tenant authorized for this policy operation.
    pub(crate) fn tenant(&self) -> &TenantId {
        self.0.tenant()
    }

    /// Credential-derived Cedar principal authorized for this operation.
    pub(crate) fn security_context(&self) -> &SecurityContext {
        self.0.security_context()
    }
}

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
        let authenticated =
            require_authenticated_context(parts.extensions.get::<AuthenticatedRequestContext>())
                .map_err(IntoResponse::into_response)?;
        require_tenant_match(authenticated, tenant).map_err(IntoResponse::into_response)?;
        match require_policy_auth(state, authenticated).await {
            Some(resp) => Err(resp),
            None => Ok(Self(authenticated.clone())),
        }
    }
}

/// GET /api/tenants/{tenant}/policies/suggestions — suggested policies from denial patterns.
async fn handle_policy_suggestions(
    State(state): State<ServerState>,
    Path(_tenant): Path<String>,
    auth: PolicyAuthed,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str();
    let suggestions = if let Some(store) = state.metadata_store_for_tenant(tenant).await {
        match store.load_policy_denial_patterns(tenant).await {
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
    let entries: Vec<serde_json::Value> = data_strings
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
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
