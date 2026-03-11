//! Shared helpers for OData request handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_odata::path::{KeyValue, ODataPath};
use temper_runtime::tenant::TenantId;

use super::constraints::{
    ConstraintViolation, post_write_invariant_checks, pre_upsert_relation_checks,
};
use crate::state::{ServerState, VerificationGateError};

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
    if let Some(val) = headers.get("x-tenant-id")
        && let Ok(s) = val.to_str()
        && !s.is_empty()
    {
        return Ok(TenantId::new(s));
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
pub(super) fn resolve_entity_type(
    state: &ServerState,
    tenant: &TenantId,
    entity_set: &str,
) -> Option<String> {
    state
        .registry
        .read()
        .unwrap() // ci-ok: infallible lock
        .resolve_entity_type(tenant, entity_set)
        .or_else(|| state.entity_set_map.get(entity_set).cloned())
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
    let violation_type = match err.violation_type {
        super::constraints::ConstraintViolationType::RelationIntegrity => "relation_integrity",
        super::constraints::ConstraintViolationType::CrossInvariant => "cross_invariant",
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
    (StatusCode::CONFLICT, axum::Json(body)).into_response()
}

/// Run pre-upsert relation checks and post-write invariant checks.
///
/// Consolidates the duplicated two-step constraint check pattern used by
/// create, patch, put, delete, and bound action handlers. The `action` label
/// is used for the post-write check (e.g. "Create", "Patch", "Put", "Delete").
pub(super) async fn run_write_prechecks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    operation: &str,
    fields: &serde_json::Value,
) -> Result<(), axum::response::Response> {
    if let Err(v) =
        pre_upsert_relation_checks(state, tenant, entity_type, entity_id, operation, fields).await
    {
        return Err(constraint_violation_response(v));
    }
    if let Err(v) = post_write_invariant_checks(
        state,
        tenant,
        entity_type,
        entity_id,
        action,
        fields,
        operation,
    )
    .await
    {
        return Err(constraint_violation_response(v));
    }
    Ok(())
}

/// Load an entity's current state or return a 404 response.
///
/// Consolidates the repeated pattern of calling `get_tenant_entity_state`
/// and mapping errors to OData error responses.
pub(super) async fn load_entity_or_404(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
) -> Result<crate::EntityResponse, axum::response::Response> {
    state
        .get_tenant_entity_state(tenant, entity_type, key)
        .await
        .map_err(|e| {
            crate::response::odata_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("Entity '{set_name}' with key '{key}' not found: {e}"),
            )
            .into_response()
        })
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
