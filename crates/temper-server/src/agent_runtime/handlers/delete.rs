//! Teardown-gated agent-run deletion handler.

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::tenant::TenantId;

use crate::agent_runtime::models::DeleteRunResponse;
use crate::state::ServerState;

use super::common::{caller_agent_context, error_response, require_auth};

/// Delete a terminal agent run after its sandbox teardown succeeds.
///
/// Deletion is an asynchronous, teardown-gated lifecycle. A request moves a
/// terminal run into `Deleting`, where `sandbox_destroyer` removes its provider
/// sandbox. The WASM callback advances it to `Deleted` only after a successful
/// teardown; failures become `DeletionFailed` and can be retried by repeating
/// this request. The entity remains event-sourced for audit but normal reads
/// return 404 after it reaches `Deleted`.
#[tracing::instrument(
    skip_all,
    fields(
        otel.name = "agent.run.delete",
        agent.run_id = %id,
    )
)]
pub(super) async fn delete_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(id): Path<String>,
) -> Response {
    let (tenant, authenticated) = match require_auth(authenticated) {
        Ok(t) => t,
        Err(r) => return *r,
    };

    let entity_state = match state
        .get_tenant_entity_state(&tenant, "TemperAgent", &id)
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(StatusCode::NOT_FOUND, &format!("Entity not found: {e}"));
        }
    };

    let action = match deletion_disposition(&entity_state.state.status) {
        DeletionDisposition::Dispatch(action) => action,
        DeletionDisposition::Deleted => {
            return authorize_deleted_delete(
                &state,
                &tenant,
                &id,
                &entity_state.state.status,
                &entity_state.state.fields,
                &authenticated,
            )
            .await;
        }
        DeletionDisposition::Active => {
            return error_response(
                StatusCode::CONFLICT,
                "only terminal agent runs can be deleted; cancel an active run first",
            );
        }
    };

    let agent_ctx = caller_agent_context(&authenticated);
    let result = state
        .dispatch_tenant_action(&tenant, "TemperAgent", &id, action, json!({}), &agent_ctx)
        .await;

    match result {
        Ok(resp) if resp.success => deletion_accepted_response(id, resp.state.status),
        Ok(resp) => {
            let message = resp
                .error
                .as_deref()
                .unwrap_or("agent-run deletion was rejected");
            deletion_race_response(&state, &tenant, &id, message).await
        }
        Err(error) => deletion_race_response(&state, &tenant, &id, &error).await,
    }
}

/// Authorize an idempotent response for a logically deleted run without
/// creating an outgoing transition from the terminal `Deleted` state.
async fn authorize_deleted_delete(
    state: &ServerState,
    tenant: &TenantId,
    run_id: &str,
    status: &str,
    fields: &serde_json::Value,
    authenticated: &AuthenticatedRequestContext,
) -> Response {
    let resource_attrs = match state
        .build_authz_resource_attrs(tenant, "TemperAgent", run_id, status, fields)
        .await
    {
        Ok(attrs) => attrs,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    match state.authorize_with_context(
        authenticated.security_context(),
        "RequestDeletion",
        "TemperAgent",
        &resource_attrs,
        tenant.as_str(),
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(denial) => error_response(StatusCode::FORBIDDEN, &denial.to_string()),
    }
}

/// Return the public asynchronous-deletion response.
fn deletion_accepted_response(run_id: String, status: String) -> Response {
    if status == "Deleted" {
        return StatusCode::NO_CONTENT.into_response();
    }
    (
        StatusCode::ACCEPTED,
        [(CONTENT_TYPE, "application/json")],
        Json(DeleteRunResponse { run_id, status }),
    )
        .into_response()
}

/// Resolve a competing DELETE request without exposing a spurious conflict.
async fn deletion_race_response(
    state: &ServerState,
    tenant: &TenantId,
    run_id: &str,
    original_error: &str,
) -> Response {
    if !is_stale_deletion_dispatch_error(original_error) {
        return error_response(StatusCode::BAD_REQUEST, original_error);
    }

    match state
        .get_tenant_entity_state(tenant, "TemperAgent", run_id)
        .await
    {
        Ok(current) => match current.state.status.as_str() {
            "Deleting" => deletion_accepted_response(run_id.to_string(), "Deleting".to_string()),
            "Deleted" => StatusCode::NO_CONTENT.into_response(),
            "DeletionFailed" => {
                deletion_accepted_response(run_id.to_string(), "DeletionFailed".to_string())
            }
            _ => error_response(StatusCode::BAD_REQUEST, original_error),
        },
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, original_error),
    }
}

/// Whether a failed dispatch can only be explained by a stale actor state.
fn is_stale_deletion_dispatch_error(error: &str) -> bool {
    error.contains("not valid from state")
        || error.contains("authorization became stale; retry against current state")
}

/// The public deletion behavior for a lifecycle status.
#[derive(Debug, PartialEq, Eq)]
enum DeletionDisposition {
    /// Dispatch this Cedar-governed action before starting teardown.
    Dispatch(&'static str),
    /// Logical deletion is terminal; authorize a no-content response directly.
    Deleted,
    /// Work is still active and must be cancelled first.
    Active,
}

/// Select the governed deletion behavior for the current lifecycle state.
fn deletion_disposition(status: &str) -> DeletionDisposition {
    match status {
        "Completed" | "Failed" | "Cancelled" => DeletionDisposition::Dispatch("RequestDeletion"),
        "DeletionFailed" | "Deleting" => DeletionDisposition::Dispatch("RetryDeletion"),
        "Deleted" => DeletionDisposition::Deleted,
        _ => DeletionDisposition::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::{DeletionDisposition, deletion_disposition, is_stale_deletion_dispatch_error};

    #[test]
    fn deletion_starts_only_from_terminal_states() {
        for status in ["Completed", "Failed", "Cancelled"] {
            assert_eq!(
                deletion_disposition(status),
                DeletionDisposition::Dispatch("RequestDeletion"),
                "{status} must begin teardown-gated deletion"
            );
        }
        assert_eq!(
            deletion_disposition("DeletionFailed"),
            DeletionDisposition::Dispatch("RetryDeletion")
        );
    }

    #[test]
    fn deletion_retries_in_progress_and_authorizes_completed_idempotence() {
        assert_eq!(
            deletion_disposition("Deleting"),
            DeletionDisposition::Dispatch("RetryDeletion")
        );
        assert_eq!(
            deletion_disposition("Deleted"),
            DeletionDisposition::Deleted
        );
    }

    #[test]
    fn race_resolution_only_accepts_known_stale_dispatch_errors() {
        assert!(is_stale_deletion_dispatch_error(
            "Action 'RequestDeletion' not valid from state 'Deleting'"
        ));
        assert!(is_stale_deletion_dispatch_error(
            "action authorization became stale; retry against current state"
        ));
        assert!(!is_stale_deletion_dispatch_error(
            "authorization denied: no matching permit policy"
        ));
        assert!(!is_stale_deletion_dispatch_error("provider request failed"));
    }

    #[test]
    fn deletion_rejects_active_states() {
        for status in [
            "Created",
            "Provisioning",
            "Thinking",
            "Executing",
            "Compacting",
            "Steering",
            "Recovering",
        ] {
            assert_eq!(
                deletion_disposition(status),
                DeletionDisposition::Active,
                "{status}"
            );
        }
    }
}
