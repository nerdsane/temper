//! Governed durable reaction retry mutation (ADR-0158).

use std::collections::BTreeMap;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};

use crate::authz::require_authenticated_context;
use crate::state::ServerState;
use crate::trigger::delivery::{append_delivery_record, find_delivery_record};

/// POST `/api/reactions/{delivery_id}/retry` — request one bounded retry.
pub(crate) async fn handle_retry_reaction(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(delivery_id): Path<String>,
) -> Response {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(context) => context,
        Err(status) => return status.into_response(),
    };
    let tenant = authenticated.tenant();
    let operator = authenticated.security_context();
    let is_explicit_human = matches!(
        operator.principal.kind,
        PrincipalKind::Customer | PrincipalKind::Admin
    ) && operator.principal.id != "anonymous";
    if !is_explicit_human {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "an explicit human operator identity is required"
            })),
        )
            .into_response();
    }
    let Some((store, _)) = state.event_journal() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (mut record, sequence) =
        match find_delivery_record(&store, tenant.as_str(), &delivery_id).await {
            Ok(Some(record)) => record,
            Ok(None) => return StatusCode::NOT_FOUND.into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };

    let resource_attrs = BTreeMap::from([
        (
            "status".to_string(),
            serde_json::json!(format!("{:?}", record.status)),
        ),
        (
            "transient_failure".to_string(),
            serde_json::json!(record.transient_failure),
        ),
        (
            "manual_retries".to_string(),
            serde_json::json!(record.manual_retries),
        ),
    ]);
    if state
        .authorize_with_context(
            operator,
            "retry_reaction",
            "ReactionDelivery",
            &resource_attrs,
            tenant.as_str(),
        )
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let retry_number = match record.request_manual_retry() {
        Ok(retry_number) => retry_number,
        Err(error) => {
            crate::runtime_metrics::record_reaction_delivery_manual_retry("rejected");
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": error})),
            )
                .into_response();
        }
    };
    if append_delivery_record(&store, sequence, &record)
        .await
        .is_err()
    {
        crate::runtime_metrics::record_reaction_delivery_manual_retry("conflict");
        return StatusCode::CONFLICT.into_response();
    }
    crate::runtime_metrics::record_reaction_delivery_manual_retry("accepted");

    let dispatcher = state
        .reaction_dispatcher
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(dispatcher) = dispatcher {
        dispatcher.notify_recovery(tenant);
        let state_for_retry = state.clone();
        let intent = record.intent.clone();
        tokio::spawn(async move {
            // determinism-ok: governed API schedules durable work; the worker uses persisted scheduler time
            if let Err(error) = dispatcher
                .dispatch_committed_intent(&state_for_retry, intent)
                .await
            {
                tracing::error!(%error, "manual reaction retry dispatch failed");
            }
        });
    }

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "delivery_id": delivery_id,
            "manual_retry": retry_number,
            "status": "pending",
        })),
    )
        .into_response()
}
