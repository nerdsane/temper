//! Authenticated registry restore quarantine inspection and repair.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tracing::instrument;

use super::RegistryOperatorAuthed;
use crate::platform_store::PlatformStore;
use crate::registry::{
    RegistryQuarantineFailure, RegistryQuarantineReason, RegistryQuarantineSource,
};
use crate::registry_bootstrap::{
    REGISTRY_QUARANTINE_ENTITY_BUDGET, RegistryRetryError, retry_registry_tenant,
};
use crate::state::ServerState;

const QUARANTINE_LIST_BUDGET: usize = 128;
const PATH_COMPONENT_BUDGET_BYTES: usize = 256;

#[derive(serde::Deserialize)]
pub(crate) struct AcknowledgeRegistryQuarantineRequest {
    spec_version: i64,
    constraint_version: Option<i64>,
}

async fn store_for_tenant(state: &ServerState, tenant: &str) -> Option<Arc<dyn PlatformStore>> {
    let stack = state.storage_stack.as_ref()?;
    if let Some(provider) = stack.turso.as_ref()
        && let Some(store) = provider.store_for_tenant(tenant).await
    {
        return Some(Arc::new(store));
    }
    stack.platform.clone()
}

fn valid_path_component(value: &str) -> bool {
    !value.is_empty() && value.len() <= PATH_COMPONENT_BUDGET_BYTES
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "error": "registry quarantine storage is unavailable"
        })),
    )
        .into_response()
}

fn acknowledged_failure(
    record: &crate::platform_store::RegistryQuarantineRecord,
) -> Option<RegistryQuarantineFailure> {
    Some(RegistryQuarantineFailure {
        spec_version: record.spec_version,
        constraint_version: record.constraint_version,
        reason: RegistryQuarantineReason::from_storage(&record.reason)?,
        source_kind: RegistryQuarantineSource::from_storage(&record.source_kind)?,
        source_line: record.source_line,
        source_column: record.source_column,
        acknowledged: true,
        detail: record.detail.clone(),
    })
}

/// GET /api/tenants/{tenant}/registry-quarantines.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/registry-quarantines"))]
pub(crate) async fn handle_list_registry_quarantines(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _auth: RegistryOperatorAuthed,
) -> Response {
    if !valid_path_component(&tenant) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(store) = store_for_tenant(&state, &tenant).await else {
        return unavailable();
    };
    let mut records = match store
        .load_registry_restore_quarantines_for_tenant(&tenant, QUARANTINE_LIST_BUDGET + 1)
        .await
    {
        Ok(records) => records,
        Err(error) => {
            tracing::error!(%error, "failed to list registry restore quarantines");
            return unavailable();
        }
    };
    let truncated = records.len() > QUARANTINE_LIST_BUDGET;
    records.truncate(QUARANTINE_LIST_BUDGET);
    let total = (!truncated).then_some(records.len());
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "tenant": tenant,
            "total": total,
            "returned": records.len(),
            "records": records,
            "truncated": truncated,
        })),
    )
        .into_response()
}

/// POST /api/tenants/{tenant}/registry-quarantines/{entity_type}/acknowledge.
#[instrument(skip_all, fields(tenant, entity_type, otel.name = "POST /api/tenants/{tenant}/registry-quarantines/{entity_type}/acknowledge"))]
pub(crate) async fn handle_acknowledge_registry_quarantine(
    State(state): State<ServerState>,
    Path((tenant, entity_type)): Path<(String, String)>,
    _auth: RegistryOperatorAuthed,
    axum::Json(request): axum::Json<AcknowledgeRegistryQuarantineRequest>,
) -> Response {
    if !valid_path_component(&tenant)
        || !valid_path_component(&entity_type)
        || request.spec_version <= 0
        || request
            .constraint_version
            .is_some_and(|version| version <= 0)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let local_identity_before = state
        .registry
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .restore_quarantine_identity(&tenant, &entity_type);
    let Some(store) = store_for_tenant(&state, &tenant).await else {
        return unavailable();
    };
    let current = match store
        .acknowledge_registry_restore_quarantine(
            &tenant,
            &entity_type,
            request.spec_version,
            request.constraint_version,
        )
        .await
    {
        Ok(current) => current,
        Err(error) => {
            tracing::error!(%error, "failed to acknowledge registry restore quarantine");
            return unavailable();
        }
    };
    let Some(current) = current else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if current != (request.spec_version, request.constraint_version) {
        return StatusCode::CONFLICT.into_response();
    }
    let records = match store
        .load_registry_restore_quarantines_for_tenant(
            &tenant,
            REGISTRY_QUARANTINE_ENTITY_BUDGET + 1,
        )
        .await
    {
        Ok(records) if records.len() <= REGISTRY_QUARANTINE_ENTITY_BUDGET => records,
        Ok(_) => return StatusCode::CONFLICT.into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to reconcile acknowledged quarantine");
            return unavailable();
        }
    };
    let Some(record) = records.iter().find(|record| {
        record.entity_type == entity_type
            && record.spec_version == request.spec_version
            && record.constraint_version == request.constraint_version
    }) else {
        return StatusCode::CONFLICT.into_response();
    };
    let Some(durable_failure) = acknowledged_failure(record) else {
        tracing::error!(
            reason = %record.reason,
            source_kind = %record.source_kind,
            "durable quarantine contains an invalid diagnostic category"
        );
        return unavailable();
    };
    let mut registry = state
        .registry
        .write()
        .unwrap_or_else(|error| error.into_inner());
    if !registry.reconcile_acknowledged_restore_quarantine(
        &tenant,
        &entity_type,
        local_identity_before,
        durable_failure,
    ) {
        tracing::warn!(
            tenant,
            entity_type,
            durable_spec_version = request.spec_version,
            "preserved a process-local quarantine that changed during acknowledgment"
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

/// POST /api/tenants/{tenant}/registry-quarantines/{entity_type}/retry.
#[instrument(skip_all, fields(tenant, entity_type, otel.name = "POST /api/tenants/{tenant}/registry-quarantines/{entity_type}/retry"))]
pub(crate) async fn handle_retry_registry_quarantine(
    State(state): State<ServerState>,
    Path((tenant, entity_type)): Path<(String, String)>,
    _auth: RegistryOperatorAuthed,
) -> Response {
    if !valid_path_component(&tenant) || !valid_path_component(&entity_type) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(store) = store_for_tenant(&state, &tenant).await else {
        return unavailable();
    };
    let report =
        match retry_registry_tenant(&state.registry, store.as_ref(), &tenant, &entity_type).await {
            Ok(report) => report,
            Err(RegistryRetryError::NotFound(error)) => {
                tracing::warn!(%error, "registry quarantine retry target disappeared");
                return StatusCode::NOT_FOUND.into_response();
            }
            Err(RegistryRetryError::Conflict(error)) => {
                tracing::warn!(%error, "registry quarantine retry lost version race");
                return StatusCode::CONFLICT.into_response();
            }
            Err(RegistryRetryError::Storage(error)) => {
                tracing::error!(%error, "registry quarantine retry failed");
                return unavailable();
            }
        };
    if report.is_healthy() {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "restored",
                "restored_specs": report.restored_specs,
            })),
        )
            .into_response();
    }
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        axum::Json(serde_json::json!({
            "status": "still_quarantined",
            "quarantined_specs": report.quarantined_spec_count(),
        })),
    )
        .into_response()
}
