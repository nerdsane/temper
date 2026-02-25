//! Shared helpers for OData request handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_odata::path::KeyValue;
use temper_runtime::tenant::TenantId;

use crate::constraint_engine::ConstraintViolation;
use crate::state::{ServerState, VerificationGateError};

/// Extract the tenant ID from request headers.
///
/// Checks `X-Tenant-Id` header first. Falls back to the first registered
/// tenant in the SpecRegistry, or `TenantId::default()` if empty.
pub(crate) fn extract_tenant(headers: &HeaderMap, state: &ServerState) -> TenantId {
    if let Some(val) = headers.get("x-tenant-id")
        && let Ok(s) = val.to_str()
        && !s.is_empty()
    {
        return TenantId::new(s);
    }

    // Fall back to the first registered tenant.
    let tenant_ids = state
        .registry
        .read()
        .unwrap() // ci-ok: infallible lock
        .tenant_ids()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if let Some(first) = tenant_ids.first() {
        return first.clone();
    }

    TenantId::default()
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
        crate::constraint_engine::ConstraintViolationType::RelationIntegrity => {
            "relation_integrity"
        }
        crate::constraint_engine::ConstraintViolationType::CrossInvariant => "cross_invariant",
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
