//! Shared helpers for OData request handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_odata::path::{KeyValue, ODataPath};
use temper_runtime::tenant::TenantId;

use super::constraints::ConstraintViolation;
use crate::response::{ODataResponse, odata_error};
use crate::state::write_record::RecordWrite;
use crate::state::{DispatchError, ServerState, VerificationGateError};

/// Extract the tenant ID from request headers.
///
/// Checks `X-Tenant-Id` header first.  In single-tenant compatibility mode
/// (the legacy default), falls back to `TenantId::default()` ("default").
/// In multi-tenant mode, rejects the request with 400 when the header is
/// missing.
pub(crate) fn extract_tenant(
    headers: &HeaderMap,
    state: &ServerState,
) -> Result<TenantId, (StatusCode, String)> {
    if let Some(value) = headers.get("x-tenant-id") {
        let tenant = value.to_str().map(str::trim).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Invalid X-Tenant-Id header encoding".to_string(),
            )
        })?;
        if !tenant.is_empty() {
            return TenantId::try_new(tenant).map_err(|error| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("Invalid X-Tenant-Id header: {error}"),
                )
            });
        }
    }

    // Multi-tenant mode: require explicit tenant header.
    if !state.single_tenant_mode {
        return Err((
            StatusCode::BAD_REQUEST,
            "Missing required X-Tenant-Id header".to_string(),
        ));
    }

    // Single-tenant compatibility: deterministic fallback to the well-known
    // default tenant rather than relying on registry registration order.
    Ok(TenantId::default())
}

pub(super) fn extract_key(key: &KeyValue) -> String {
    match key {
        KeyValue::Single(k) => k.clone(),
        KeyValue::Composite(pairs) => pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

pub(super) fn has_expand_options(options: &temper_odata::query::types::ExpandOptions) -> bool {
    options.select.is_some()
        || options.filter.is_some()
        || options.orderby.is_some()
        || options.top.is_some()
        || options.skip.is_some()
        || options.expand.is_some()
}

/// Resolve an entity set name to an entity type for a tenant.
///
/// Tries SpecRegistry first, then legacy entity_set_map.
pub(crate) fn resolve_entity_type(
    state: &ServerState,
    tenant: &TenantId,
    entity_set: &str,
) -> Option<String> {
    let reg_result = state
        .registry
        .read()
        .unwrap() // ci-ok: RwLock read — poisoned lock = prior panic, fail-fast correct
        .resolve_entity_type(tenant, entity_set);
    let legacy_result = state.entity_set_map.get(entity_set).cloned();
    let result = reg_result.or(legacy_result);
    if result.is_none() {
        let reg = state.registry.read().unwrap(); // ci-ok: RwLock read — poisoned lock = prior panic, fail-fast correct
        let tenant_exists = reg.get_tenant(tenant).is_some();
        let map_size = reg
            .get_tenant(tenant)
            .map(|tc| tc.entity_set_map.len())
            .unwrap_or(0);
        tracing::warn!(
            tenant = %tenant,
            entity_set = %entity_set,
            tenant_exists,
            map_size,
            "entity_set_not_found"
        );
    }
    result
}

/// Get the CSDL XML for a tenant.
///
/// Tries SpecRegistry first, then legacy csdl_xml.
pub(super) fn tenant_csdl_xml(state: &ServerState, tenant: &TenantId) -> String {
    state
        .registry
        .read()
        .unwrap() // ci-ok: infallible lock
        .get_tenant(tenant)
        .map(|tc| tc.csdl_xml.as_ref().clone())
        .unwrap_or_else(|| state.csdl_xml.as_ref().clone())
}

/// List entity sets for a tenant.
///
/// Tries SpecRegistry first, then legacy entity_set_map.
pub(super) fn tenant_entity_sets(state: &ServerState, tenant: &TenantId) -> Vec<String> {
    let registry = state.registry.read().unwrap();
    if let Some(tc) = registry.get_tenant(tenant) {
        tc.entity_set_map.keys().cloned().collect()
    } else {
        state.entity_set_map.keys().cloned().collect()
    }
}

/// Build an HTTP 423 Locked response from a verification gate error.
pub(super) fn verification_gate_response(err: VerificationGateError) -> axum::response::Response {
    let body = serde_json::json!({
        "error": {
            "code": "VerificationRequired",
            "message": err.message,
            "details": {
                "verification_status": err.status,
                "entity_type": err.entity_type,
                "failed_levels": err.failed_levels,
            }
        }
    });
    (StatusCode::LOCKED, axum::Json(body)).into_response()
}

pub(super) fn constraint_violation_response(err: ConstraintViolation) -> axum::response::Response {
    // An unverified relation is a check the server could not finish, not a
    // conflict with the entity's data, so the caller is told to retry.
    let (violation_type, status) = match err.violation_type {
        super::constraints::ConstraintViolationType::RelationIntegrity => {
            ("relation_integrity", StatusCode::CONFLICT)
        }
        super::constraints::ConstraintViolationType::RelationUnverified => {
            ("relation_unverified", StatusCode::SERVICE_UNAVAILABLE)
        }
        super::constraints::ConstraintViolationType::CrossInvariant => {
            ("cross_invariant", StatusCode::CONFLICT)
        }
        super::constraints::ConstraintViolationType::StoredValueUnreadable => {
            ("stored_value_unreadable", StatusCode::CONFLICT)
        }
        super::constraints::ConstraintViolationType::FieldInvariant => {
            ("field_invariant", StatusCode::CONFLICT)
        }
    };
    let body = serde_json::json!({
        "error": {
            "code": "ConstraintViolation",
            "message": err.message,
            "details": {
                "type": violation_type,
                "invariant": err.invariant,
                "entity_type": err.entity_type,
                "entity_id": err.entity_id,
                "operation": err.operation,
            }
        }
    });
    (status, axum::Json(body)).into_response()
}

/// Check the record a write will store, answering a violation with 409
/// `ConstraintViolation`.
///
/// `record` is the record as stored (see `state::write_record`), never the
/// request body. The `action` label selects cross-invariants (e.g. "Create",
/// "Patch", "Put", "Delete").
pub(super) async fn run_write_prechecks(
    state: &ServerState,
    write: RecordWrite<'_>,
    record: &serde_json::Value,
) -> Result<(), axum::response::Response> {
    state
        .check_record_to_store(write, record)
        .await
        .map_err(constraint_violation_response)
}

/// Resolve the parent of a `$value` path to `(set_name, entity_id)`.
///
/// Returns 400 if the parent is not an entity instance.
#[allow(clippy::result_large_err)]
pub(super) fn resolve_value_parent(
    parent: &ODataPath,
) -> Result<(String, String), axum::response::Response> {
    match parent {
        ODataPath::Entity(set_name, key) => Ok((set_name.clone(), extract_key(key))),
        _ => Err(crate::response::odata_error(
            StatusCode::BAD_REQUEST,
            "InvalidPath",
            "$value must follow an entity instance, e.g. /Files('id')/$value",
        )
        .into_response()),
    }
}

/// Check that an entity type has `HasStream=true` in its CSDL definition.
///
/// Returns 400 if the entity type does not support `$value`.
#[allow(clippy::result_large_err)]
pub(super) fn check_has_stream_or_400(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Result<(), axum::response::Response> {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let has_stream = registry
        .get_tenant(tenant)
        .map(|tc| {
            tc.csdl
                .schemas
                .iter()
                .flat_map(|s| &s.entity_types)
                .any(|et| et.name == entity_type && et.has_stream)
        })
        .unwrap_or(false);
    if has_stream {
        Ok(())
    } else {
        Err(crate::response::odata_error(
            StatusCode::BAD_REQUEST,
            "NotAMediaEntity",
            &format!("Entity type '{entity_type}' does not support $value (HasStream=false)"),
        )
        .into_response())
    }
}

/// The answer when a write's record could not be read back for its decision
/// (`ServerState::materialize_authorized_snapshot`).
///
/// A conflict is the record created differently than the write's checks read
/// it: 409 `AuthorizationStateChanged`, a write that is decided again. Any
/// other error is the read itself failing, such as an actor ask that timed
/// out: 500 `ReadError`, the answer the same read gets before the decision,
/// because the record did not change.
pub(super) fn authorized_snapshot_failure(error: &DispatchError) -> ODataResponse {
    let (status, code) = match error {
        DispatchError::Conflict(_) => (StatusCode::CONFLICT, "AuthorizationStateChanged"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "ReadError"),
    };
    odata_error(status, code, &error.to_string())
}

#[cfg(test)]
#[path = "common_test.rs"]
mod authorized_snapshot_failure_tests;
