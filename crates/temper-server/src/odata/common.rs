//! Shared helpers for OData request handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use opentelemetry::KeyValue as OtelKeyValue;
use opentelemetry::trace::{Span, Tracer};
use std::collections::BTreeMap;
use temper_odata::path::{KeyValue, ODataPath};
use temper_runtime::tenant::TenantId;

use super::constraints::{
    ConstraintViolation, post_write_invariant_checks, pre_upsert_field_invariant_checks,
    pre_upsert_relation_checks,
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

pub(super) fn directed_evolution_header_fields(
    headers: &HeaderMap,
) -> BTreeMap<&'static str, String> {
    let mut fields = BTreeMap::new();
    for (header, field) in [
        ("x-de-episode-id", "episode_id"),
        ("x-de-direction-id", "direction_id"),
        ("x-de-generation-id", "generation_id"),
        ("x-de-variant-id", "variant_id"),
        ("x-de-stage-id", "stage_id"),
        ("x-de-stage-result-id", "stage_result_id"),
        ("x-de-trial-id", "trial_id"),
        ("x-de-persona-index", "persona_index"),
        ("x-de-run-index", "run_index"),
        ("x-de-simulated-user-id", "simulated_user_id"),
        ("x-de-work-item-id", "work_item_id"),
        ("x-de-runtime-ref", "runtime_ref"),
        ("x-de-app-ref", "app_ref"),
    ] {
        if let Some(value) = headers
            .get(header)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            fields.insert(field, value.to_string());
        }
    }
    fields
}

pub(super) fn log_directed_evolution_odata_request(
    headers: &HeaderMap,
    tenant: &TenantId,
    method: &str,
    path: &str,
) {
    let fields = directed_evolution_header_fields(headers);
    if fields.is_empty() {
        return;
    }
    crate::runtime_metrics::record_directed_evolution_runtime_request(
        tenant.as_str(),
        method,
        path,
        &fields,
    );
    let mut otel_fields = vec![
        OtelKeyValue::new("directed_evolution", true),
        OtelKeyValue::new("tenant", tenant.as_str().to_string()),
        OtelKeyValue::new("http.method", method.to_string()),
        OtelKeyValue::new("odata.path", path.to_string()),
    ];
    for (field, value) in &fields {
        otel_fields.push(OtelKeyValue::new(
            format!("directed_evolution.{field}"),
            value.clone(),
        ));
    }
    let tracer = opentelemetry::global::tracer("temper");
    let mut span = tracer
        .span_builder("directed_evolution.runtime_request")
        .with_attributes(otel_fields)
        .start(&tracer);
    span.end();
    tracing::info!(
        directed_evolution = true,
        tenant = %tenant,
        http.method = %method,
        odata.path = %path,
        directed_evolution.episode_id = %fields.get("episode_id").map(String::as_str).unwrap_or(""),
        directed_evolution.direction_id = %fields.get("direction_id").map(String::as_str).unwrap_or(""),
        directed_evolution.generation_id = %fields.get("generation_id").map(String::as_str).unwrap_or(""),
        directed_evolution.variant_id = %fields.get("variant_id").map(String::as_str).unwrap_or(""),
        directed_evolution.stage_id = %fields.get("stage_id").map(String::as_str).unwrap_or(""),
        directed_evolution.stage_result_id = %fields.get("stage_result_id").map(String::as_str).unwrap_or(""),
        directed_evolution.trial_id = %fields.get("trial_id").map(String::as_str).unwrap_or(""),
        directed_evolution.persona_index = %fields.get("persona_index").map(String::as_str).unwrap_or(""),
        directed_evolution.run_index = %fields.get("run_index").map(String::as_str).unwrap_or(""),
        directed_evolution.simulated_user_id = %fields.get("simulated_user_id").map(String::as_str).unwrap_or(""),
        directed_evolution.work_item_id = %fields.get("work_item_id").map(String::as_str).unwrap_or(""),
        directed_evolution.runtime_ref = %fields.get("runtime_ref").map(String::as_str).unwrap_or(""),
        directed_evolution.app_ref = %fields.get("app_ref").map(String::as_str).unwrap_or(""),
        "directed evolution runtime request"
    );
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
    let violation_type = match err.violation_type {
        super::constraints::ConstraintViolationType::RelationIntegrity => "relation_integrity",
        super::constraints::ConstraintViolationType::CrossInvariant => "cross_invariant",
        super::constraints::ConstraintViolationType::FieldInvariant => "field_invariant",
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
    if let Err(v) =
        pre_upsert_field_invariant_checks(state, tenant, entity_type, entity_id, operation, fields)
            .await
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
