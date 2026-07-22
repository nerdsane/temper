//! WASM module management endpoints.
//!
//! Upload, download, delete, and list WASM integration modules.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use temper_runtime::tenant::TenantId;

use tracing::instrument;

use crate::authz::{
    GovernedMutationAuth, observe_tenant_scope, require_governed_mutation_auth,
    require_observe_auth,
};
use crate::odata::extract_tenant;
use crate::state::{ServerState, SpecPublicationGuard};

mod list;

pub use list::{handle_list_wasm_invocations, handle_list_wasm_modules};

async fn begin_wasm_generation_mutation(
    state: &ServerState,
    tenant: &TenantId,
) -> Result<SpecPublicationGuard, (StatusCode, String)> {
    let guard = state
        .begin_spec_publication(tenant)
        .await
        .map_err(|error| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("tenant runtime generation is busy: {error}"),
            )
        })?;
    Ok(guard)
}

async fn begin_wasm_generation_read(
    state: &ServerState,
    tenant: &TenantId,
) -> Result<tokio::sync::OwnedRwLockReadGuard<()>, StatusCode> {
    if state.spec_publication_gated(tenant) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let guard = state
        .try_begin_tenant_request(tenant)
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    if state.spec_publication_gated(tenant) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    Ok(guard)
}

async fn authorize_wasm_mutation(
    state: &ServerState,
    headers: &HeaderMap,
    tenant: &TenantId,
    module_name: &str,
) -> Result<(), (StatusCode, String)> {
    let mut resource_attrs = BTreeMap::new();
    resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(module_name.to_string()),
    );
    resource_attrs.insert(
        "module_name".to_string(),
        serde_json::Value::String(module_name.to_string()),
    );
    if let Some(response) = require_governed_mutation_auth(
        state,
        headers,
        GovernedMutationAuth {
            tenant: tenant.as_str(),
            action: "manage_wasm",
            resource_type: "WasmModule",
            resource_id: module_name,
            resource_attrs,
            module_name: Some(module_name),
            from_status: None,
        },
    )
    .await
    {
        return Err(response);
    }
    Ok(())
}

fn known_wasm_tenants(state: &ServerState) -> Result<BTreeSet<String>, StatusCode> {
    let mut tenants = state
        .registry
        .read()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .tenant_ids()
        .into_iter()
        .map(|tenant| tenant.as_str().to_string())
        .collect::<BTreeSet<_>>();
    tenants.extend(
        state
            .wasm_module_registry
            .read()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .all_modules()
            .into_iter()
            .map(|(tenant, _, _)| tenant.to_string()),
    );
    Ok(tenants)
}

async fn begin_all_wasm_generation_reads(
    state: &ServerState,
    tenants: &BTreeSet<String>,
) -> Result<Vec<tokio::sync::OwnedRwLockReadGuard<()>>, StatusCode> {
    let mut guards = Vec::with_capacity(tenants.len());
    for tenant in tenants {
        guards.push(begin_wasm_generation_read(state, &TenantId::new(tenant)).await?);
    }
    Ok(guards)
}

fn wasm_authorization_tenant(headers: &HeaderMap) -> String {
    headers
        .get("x-tenant-id")
        .and_then(|value| value.to_str().ok())
        .filter(|tenant| !tenant.is_empty())
        .unwrap_or("system")
        .to_string()
}

#[derive(Deserialize)]
struct WasmModuleUploadJson {
    wasm_base64: String,
}

/// Response for WASM module upload.
#[derive(Serialize)]
pub struct WasmModuleUploadResponse {
    /// Module name as registered.
    pub module_name: String,
    /// SHA-256 hash of the module bytes.
    pub sha256_hash: String,
    /// Size of the uploaded module in bytes.
    pub size_bytes: usize,
}

/// Response for WASM module info.
#[derive(Serialize)]
pub struct WasmModuleInfoResponse {
    /// Module name.
    pub module_name: String,
    /// SHA-256 hash of the module bytes.
    pub sha256_hash: String,
    /// Whether the compiled module is in the engine cache.
    pub cached: bool,
}

/// Entry in the module list response (with stats).
#[derive(Serialize)]
pub struct WasmModuleListEntry {
    /// Tenant that owns this module.
    pub tenant: String,
    /// Module name.
    pub module_name: String,
    /// SHA-256 hash of the module bytes.
    pub sha256_hash: String,
    /// Whether the compiled module is in the engine cache.
    pub cached: bool,
    /// Total invocations recorded in the bounded log.
    pub total_invocations: usize,
    /// Successful invocations in the bounded log.
    pub success_count: usize,
    /// Success rate (0.0-1.0).
    pub success_rate: f64,
    /// Last invocation timestamp (if any).
    pub last_invoked_at: Option<String>,
}

/// Query parameters for the invocations endpoint.
#[derive(Deserialize)]
pub struct InvocationQueryParams {
    /// Filter by module name.
    pub module_name: Option<String>,
    /// Filter by success status.
    pub success: Option<bool>,
    /// Max entries to return (default: 100).
    pub limit: Option<usize>,
}

/// Serialized invocation entry for the API response.
#[derive(Serialize)]
pub struct WasmInvocationResponse {
    /// Invocation entries matching the query.
    pub invocations: Vec<serde_json::Value>,
    /// Total count of matching entries.
    pub total: usize,
}

/// POST /api/wasm/modules/{module_name} — upload a WASM binary.
///
/// Admin principals bypass Cedar; other principals require "manage_wasm" on "WasmModule".
#[instrument(skip_all, fields(module_name, otel.name = "POST /api/wasm/modules/{module_name}"))]
pub async fn handle_upload_wasm_module(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(module_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<WasmModuleUploadResponse>, (StatusCode, String)> {
    let tenant = extract_tenant(&headers, &state)?;
    authorize_wasm_mutation(&state, &headers, &tenant, &module_name).await?;

    let module_bytes = decode_wasm_upload_body(&headers, body)?;

    // TigerStyle: pre-assertion on module size (10 MB budget)
    if module_bytes.len() > temper_wasm::types::MAX_MODULE_SIZE {
        tracing::warn!(size = module_bytes.len(), "WASM module too large");
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "WASM module too large: {} bytes (max {})",
                module_bytes.len(),
                temper_wasm::types::MAX_MODULE_SIZE
            ),
        ));
    }

    // Compile and cache (must succeed before persisting)
    let hash = state
        .wasm_engine
        .compile_and_cache(&module_bytes)
        .map_err(|e| {
            tracing::warn!(error = %e, "WASM compilation failed");
            (
                StatusCode::BAD_REQUEST,
                format!("WASM compilation failed: {e}"),
            )
        })?;

    // Serialize the durable row and live module-name mapping with spec/app
    // publication. Compilation above is content-addressed cache warming only;
    // no tenant-visible generation changes before this writer is held.
    let mut generation_writer = begin_wasm_generation_mutation(&state, &tenant).await?;
    authorize_wasm_mutation(&state, &headers, &tenant, &module_name).await?;
    let intent = ServerState::spec_publication_intent(
        "direct-wasm-upload-v1",
        [
            ("module-name", module_name.as_bytes()),
            ("sha256", hash.as_bytes()),
            ("wasm-bytes", module_bytes.as_ref()),
        ],
    );
    state
        .arm_spec_publication(&mut generation_writer, &tenant, &intent)
        .map_err(|error| (StatusCode::CONFLICT, error))?;

    // Persist to durable storage first — if durability fails, refuse the upload.
    // This ensures the module survives restarts before we expose it in memory.
    // source="upload" so the os-apps install pipeline won't clobber this row at
    // next boot.
    if let Err(e) = state
        .upsert_wasm_module(
            tenant.as_str(),
            &module_name,
            &module_bytes,
            &hash,
            "upload",
        )
        .await
    {
        tracing::error!(error = %e, "failed to persist WASM module to durable store");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to persist WASM module: {e}"),
        ));
    }

    // Register in module registry after durability is confirmed.
    {
        let mut wasm_reg = state.wasm_module_registry.write().map_err(|error| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("WASM registry lock poisoned: {error}"),
            )
        })?;
        wasm_reg.register(&tenant, &module_name, &hash);
    }
    state
        .complete_spec_publication_retry(&mut generation_writer, &tenant)
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error))?;

    let size_bytes = module_bytes.len();
    tracing::info!(
        tenant = %tenant,
        module = %module_name,
        hash = %hash,
        size = size_bytes,
        "WASM module uploaded and cached"
    );

    Ok(Json(WasmModuleUploadResponse {
        module_name,
        sha256_hash: hash,
        size_bytes,
    }))
}

fn decode_wasm_upload_body(
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::body::Bytes, (StatusCode, String)> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .any(|part| part.trim().eq_ignore_ascii_case("application/json"))
    {
        return Ok(body);
    }

    let payload: WasmModuleUploadJson = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid WASM upload JSON body: {e}"),
        )
    })?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload.wasm_base64)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid wasm_base64 payload: {e}"),
            )
        })?;
    Ok(axum::body::Bytes::from(decoded))
}

/// GET /observe/wasm/modules/{module_name} — module info.
#[instrument(skip_all, fields(module_name, otel.name = "GET /observe/wasm/modules/{module_name}"))]
pub async fn handle_get_wasm_module_info(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(module_name): Path<String>,
) -> Result<Json<WasmModuleInfoResponse>, StatusCode> {
    let tenant = extract_tenant(&headers, &state).map_err(|(s, _)| s)?;
    let _generation = begin_wasm_generation_read(&state, &tenant).await?;

    let hash = {
        let wasm_reg = state.wasm_module_registry.read().unwrap();
        wasm_reg
            .get_hash(&tenant, &module_name)
            .map(|s| s.to_string())
    };

    let Some(hash) = hash else {
        tracing::warn!("WASM module not found");
        return Err(StatusCode::NOT_FOUND);
    };

    let cached = state.wasm_engine.is_cached(&hash);

    Ok(Json(WasmModuleInfoResponse {
        module_name,
        sha256_hash: hash,
        cached,
    }))
}

/// DELETE /api/wasm/modules/{module_name} — remove a module.
///
/// Admin principals bypass Cedar; other principals require "manage_wasm" on "WasmModule".
#[instrument(skip_all, fields(module_name, otel.name = "DELETE /api/wasm/modules/{module_name}"))]
pub async fn handle_delete_wasm_module(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(module_name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let tenant = extract_tenant(&headers, &state)?;
    authorize_wasm_mutation(&state, &headers, &tenant, &module_name).await?;

    let mut generation_writer = begin_wasm_generation_mutation(&state, &tenant).await?;
    authorize_wasm_mutation(&state, &headers, &tenant, &module_name).await?;
    let retrying_exact_delete = state.spec_publication_gated(&tenant);

    // Get hash after joining the tenant-generation writer, before removing it
    // from the registry for cache eviction.
    let hash = {
        let wasm_reg = state.wasm_module_registry.read().unwrap();
        wasm_reg
            .get_hash(&tenant, &module_name)
            .map(|s| s.to_string())
    };

    if hash.is_none() && !retrying_exact_delete {
        tracing::warn!("WASM module not found for deletion");
        return Err((
            StatusCode::NOT_FOUND,
            format!("WASM module '{module_name}' not found for tenant '{tenant}'"),
        ));
    }

    let intent = ServerState::spec_publication_intent(
        "direct-wasm-delete-v1",
        [("module-name", module_name.as_bytes())],
    );
    state
        .arm_spec_publication(&mut generation_writer, &tenant, &intent)
        .map_err(|error| (StatusCode::CONFLICT, error))?;

    // Delete from durable storage first — if durability fails, refuse the delete
    // so memory stays consistent with the durable store.
    if let Err(e) = state
        .delete_wasm_module(tenant.as_str(), &module_name)
        .await
    {
        tracing::error!(error = %e, "failed to delete WASM module from durable store");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to delete WASM module from durable store: {e}"),
        ));
    }

    // Remove from in-memory registry after durability is confirmed.
    {
        let mut wasm_reg = state.wasm_module_registry.write().map_err(|error| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("WASM registry lock poisoned: {error}"),
            )
        })?;
        wasm_reg.remove(&tenant, &module_name);
    }

    // Evict from engine cache last.
    if let Some(ref hash) = hash {
        state.wasm_engine.evict(hash);
    }
    state
        .complete_spec_publication_retry(&mut generation_writer, &tenant)
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error))?;

    tracing::info!(
        tenant = %tenant,
        module = %module_name,
        "WASM module deleted"
    );

    Ok(Json(serde_json::json!({
        "deleted": true,
        "module_name": module_name,
    })))
}
