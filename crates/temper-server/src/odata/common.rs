//! Shared helpers for OData request handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_odata::path::{KeyValue, ODataPath};
use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, SchemaScope, SchemaScopeKind, is_canonical_sha256_digest,
};
use temper_runtime::tenant::TenantId;

use super::constraints::{
    ConstraintViolation, post_write_invariant_checks, pre_upsert_field_invariant_checks,
    pre_upsert_relation_checks,
};
use super::schema_pin::schema_pin_mismatch_response;
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

/// Resolve an optional task scope to its immutable active bundle.
///
/// Both headers are required together. A declared scope never silently falls
/// back to tenant-global behavior; that path is enabled only by the registry's
/// explicit compatibility bit.
pub(crate) async fn extract_schema_pin(
    headers: &HeaderMap,
    state: &ServerState,
    tenant: &TenantId,
) -> Result<Option<SchemaExecutionPin>, (StatusCode, String)> {
    let kind = headers.get("x-temper-schema-scope-kind");
    let id = headers.get("x-temper-schema-scope-id");
    let requested_digest = headers.get("x-temper-schema-bundle-digest");
    let (kind, id) = match (kind, id) {
        (None, None) if requested_digest.is_none() => return Ok(None),
        (None, None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "Schema bundle digest requires schema scope kind and id".to_string(),
            ));
        }
        (Some(kind), Some(id)) => {
            let kind = kind.to_str().map(str::trim).map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "Schema scope kind must be valid UTF-8".to_string(),
                )
            })?;
            let id = id.to_str().map(str::trim).map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "Schema scope id must be valid UTF-8".to_string(),
                )
            })?;
            if kind.is_empty() || id.is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Schema scope kind and id must be non-empty".to_string(),
                ));
            }
            (kind, id)
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "Schema scope kind and id must be supplied together".to_string(),
            ));
        }
    };
    if kind != "task" {
        return Err((
            StatusCode::BAD_REQUEST,
            "Unsupported schema scope kind".to_string(),
        ));
    }
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: id.to_string(),
    };
    let requested_digest = requested_digest
        .map(|value| {
            value
                .to_str()
                .map(str::trim)
                .map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        "Schema bundle digest must be valid UTF-8".to_string(),
                    )
                })
                .and_then(|digest| {
                    if is_canonical_sha256_digest(digest) {
                        Ok(digest.to_string())
                    } else {
                        Err((
                            StatusCode::BAD_REQUEST,
                            "Schema bundle digest must use canonical sha256:<64 lowercase hex> form"
                                .to_string(),
                        ))
                    }
                })
        })
        .transpose()?;
    if let Some(bundle_digest) = requested_digest {
        let exact_bundle_loaded = state
            .registry
            .read()
            .expect("registry lock poisoned")
            .get_scoped_config_at_digest(tenant, &scope, &bundle_digest)
            .is_some();
        if !exact_bundle_loaded {
            crate::schema_deployment::GovernedSchemaDeploymentService::new(state)
                .recover_registry_bundle(tenant.as_str(), &scope, &bundle_digest)
                .await
                .map_err(|error| {
                    (
                        StatusCode::CONFLICT,
                        format!(
                            "{} {}",
                            crate::state::SCHEMA_PIN_MISMATCH_PREFIX,
                            error.message()
                        ),
                    )
                })?;
        }
        return Ok(Some(SchemaExecutionPin {
            scope,
            bundle_digest,
        }));
    }
    if state
        .registry
        .read()
        .expect("registry lock poisoned")
        .active_scope_digest(tenant, &scope)
        .is_none()
        && state
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.schema_deployments.as_ref())
            .is_some()
    {
        crate::schema_deployment::GovernedSchemaDeploymentService::new(state)
            .recover_registry_pointer(tenant.as_str(), &scope)
            .await
            .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error.message().to_string()))?;
    }
    let registry = state.registry.read().expect("registry lock poisoned");
    if let Some(bundle_digest) = registry.active_scope_digest(tenant, &scope) {
        return Ok(Some(SchemaExecutionPin {
            scope,
            bundle_digest: bundle_digest.to_string(),
        }));
    }
    if registry.scope_allows_global_compatibility(tenant, &scope) {
        return Ok(None);
    }
    Err((
        StatusCode::CONFLICT,
        "Schema scope has no active bundle".to_string(),
    ))
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

/// Resolve against an exact immutable bundle when a scoped pin is present.
pub(super) fn resolve_entity_type_for_pin(
    state: &ServerState,
    tenant: &TenantId,
    schema_pin: Option<&SchemaExecutionPin>,
    entity_set: &str,
) -> Option<String> {
    match schema_pin {
        Some(pin) => state
            .registry
            .read()
            .expect("registry lock poisoned")
            .get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            .and_then(|config| config.entity_set_map.get(entity_set).cloned()),
        None => resolve_entity_type(state, tenant, entity_set),
    }
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

pub(super) fn tenant_csdl_xml_for_pin(
    state: &ServerState,
    tenant: &TenantId,
    schema_pin: Option<&SchemaExecutionPin>,
) -> Option<String> {
    match schema_pin {
        Some(pin) => state
            .registry
            .read()
            .expect("registry lock poisoned")
            .get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            .map(|config| config.csdl_xml.as_ref().clone()),
        None => Some(tenant_csdl_xml(state, tenant)),
    }
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

pub(super) fn tenant_entity_sets_for_pin(
    state: &ServerState,
    tenant: &TenantId,
    schema_pin: Option<&SchemaExecutionPin>,
) -> Option<Vec<String>> {
    match schema_pin {
        Some(pin) => state
            .registry
            .read()
            .expect("registry lock poisoned")
            .get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            .map(|config| config.entity_set_map.keys().cloned().collect()),
        None => Some(tenant_entity_sets(state, tenant)),
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
pub(crate) async fn run_write_prechecks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    labels: (&str, &str),
    fields: &serde_json::Value,
    schema_pin: Option<&SchemaExecutionPin>,
) -> Result<(), axum::response::Response> {
    let (action, operation) = labels;
    if let Err(v) = pre_upsert_relation_checks(
        state,
        tenant,
        entity_type,
        entity_id,
        operation,
        fields,
        schema_pin,
    )
    .await
    {
        return Err(constraint_violation_response(v));
    }
    if let Err(v) = pre_upsert_field_invariant_checks(
        state,
        tenant,
        entity_type,
        entity_id,
        operation,
        fields,
        schema_pin,
    )
    .await
    {
        return Err(constraint_violation_response(v));
    }
    if let Err(v) = post_write_invariant_checks(
        state,
        tenant,
        entity_type,
        entity_id,
        (action, operation),
        fields,
        schema_pin,
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
#[expect(
    dead_code,
    reason = "shared by the next OData mutation call-site migration"
)]
pub(super) async fn load_entity_or_404(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    set_name: &str,
    key: &str,
    schema_pin: Option<&SchemaExecutionPin>,
) -> Result<crate::EntityResponse, axum::response::Response> {
    let result = match schema_pin {
        Some(pin) => {
            state
                .get_scoped_entity_state(tenant, entity_type, key, pin.clone())
                .await
        }
        None => {
            crate::application_data::GovernedApplicationDataService::new(state)
                .get(tenant, entity_type, key)
                .await
        }
    };
    result.map_err(|error| {
        schema_pin_mismatch_response(&error).unwrap_or_else(|| {
            crate::response::odata_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("Entity '{set_name}' with key '{key}' not found: {error}"),
            )
            .into_response()
        })
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
