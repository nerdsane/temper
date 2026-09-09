//! WASM module management endpoints.
//!
//! Upload, download, delete, and list WASM integration modules.

use std::collections::BTreeMap;

use axum::extract::Path;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use temper_authz::AuthenticatedRequestContext;

use tracing::instrument;

use crate::authz::{
    GovernedMutationAuth, observe_tenant_scope, require_authenticated_context,
    require_governed_mutation_auth, require_observe_auth,
};
use crate::state::ServerState;

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
/// Requires `manage_wasm` on the exact `WasmModule` resource.
#[instrument(skip_all, fields(module_name, otel.name = "POST /api/wasm/modules/{module_name}"))]
pub async fn handle_upload_wasm_module(
    State(state): State<ServerState>,
    headers: HeaderMap,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(module_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<WasmModuleUploadResponse>, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "authentication required".to_string()))?;
    let tenant = authenticated.tenant().clone();
    let mut resource_attrs = BTreeMap::new();
    resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(module_name.clone()),
    );
    resource_attrs.insert(
        "module_name".to_string(),
        serde_json::Value::String(module_name.clone()),
    );
    if let Some(resp) = require_governed_mutation_auth(
        &state,
        authenticated,
        GovernedMutationAuth {
            tenant: tenant.as_str(),
            action: "manage_wasm",
            resource_type: "WasmModule",
            resource_id: &module_name,
            resource_attrs,
            module_name: Some(&module_name),
            from_status: None,
        },
    )
    .await
    {
        return Err(resp);
    }

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
        state.wasm_engine.evict(&hash);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to persist WASM module: {e}"),
        ));
    }

    // Register in module registry after durability is confirmed.
    {
        let mut wasm_reg = state.wasm_module_registry.write().unwrap();
        wasm_reg.register(&tenant, &module_name, &hash);
    }

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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(module_name): Path<String>,
) -> Result<Json<WasmModuleInfoResponse>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_wasm", "WasmModule")?;
    let tenant = authenticated.tenant().clone();

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
/// Requires `manage_wasm` on the exact `WasmModule` resource.
#[instrument(skip_all, fields(module_name, otel.name = "DELETE /api/wasm/modules/{module_name}"))]
pub async fn handle_delete_wasm_module(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(module_name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "authentication required".to_string()))?;
    let tenant = authenticated.tenant().clone();
    let mut resource_attrs = BTreeMap::new();
    resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(module_name.clone()),
    );
    resource_attrs.insert(
        "module_name".to_string(),
        serde_json::Value::String(module_name.clone()),
    );
    if let Some(resp) = require_governed_mutation_auth(
        &state,
        authenticated,
        GovernedMutationAuth {
            tenant: tenant.as_str(),
            action: "manage_wasm",
            resource_type: "WasmModule",
            resource_id: &module_name,
            resource_attrs,
            module_name: Some(&module_name),
            from_status: None,
        },
    )
    .await
    {
        return Err(resp);
    }

    // Get hash before removing from registry (for cache eviction)
    let hash = {
        let wasm_reg = state.wasm_module_registry.read().unwrap();
        wasm_reg
            .get_hash(&tenant, &module_name)
            .map(|s| s.to_string())
    };

    if hash.is_none() {
        tracing::warn!("WASM module not found for deletion");
        return Err((
            StatusCode::NOT_FOUND,
            format!("WASM module '{module_name}' not found for tenant '{tenant}'"),
        ));
    }

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
        let mut wasm_reg = state.wasm_module_registry.write().unwrap();
        wasm_reg.remove(&tenant, &module_name);
    }

    // Evict from engine cache last.
    if let Some(ref hash) = hash {
        state.wasm_engine.evict(hash);
    }

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

/// GET /observe/wasm/modules — list modules in the credential-bound tenant.
#[instrument(skip_all, fields(otel.name = "GET /observe/wasm/modules"))]
pub async fn handle_list_wasm_modules(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_wasm", "WasmModule")?;
    let tenant_scope = observe_tenant_scope(authenticated);

    // Collect invocation stats via fan-out across all tenant stores.
    let invocation_stats: std::collections::BTreeMap<String, (usize, usize, Option<String>)> = {
        let mut stats: std::collections::BTreeMap<String, (usize, usize, Option<String>)> =
            std::collections::BTreeMap::new();
        let store = state
            .metadata_store_for_tenant(authenticated.tenant().as_str())
            .await;
        for store in store.iter() {
            if let Ok(rows) = store.load_recent_wasm_invocations(10_000).await {
                for row in rows {
                    let module = row.module_name.clone();
                    let success = row.success;
                    let ts = Some(row.created_at.clone());
                    let (total, s_count, last_ts) = stats.entry(module).or_insert((0, 0, None));
                    *total += 1;
                    if success {
                        *s_count += 1;
                    }
                    if ts.is_some() {
                        *last_ts = ts;
                    }
                }
            }
        }
        stats
    };

    let modules: Vec<WasmModuleListEntry> = {
        let wasm_reg = state.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock

        let make_entry = |tenant: &str, name: &str, hash: &str| {
            let cached = state.wasm_engine.is_cached(hash);
            let (total_invocations, success_count, last_invoked_at) =
                invocation_stats.get(name).cloned().unwrap_or((0, 0, None));
            let success_rate = if total_invocations > 0 {
                success_count as f64 / total_invocations as f64
            } else {
                0.0
            };
            WasmModuleListEntry {
                tenant: tenant.to_string(),
                module_name: name.to_string(),
                sha256_hash: hash.to_string(),
                cached,
                total_invocations,
                success_count,
                success_rate,
                last_invoked_at,
            }
        };

        let mut entries: Vec<WasmModuleListEntry> = wasm_reg
            .all_modules()
            .into_iter()
            .filter(|(tenant, _, _)| tenant_scope.as_str() == *tenant)
            .map(|(tenant, name, hash)| make_entry(tenant, name, hash))
            .collect();

        // Include built-in modules (visible to all tenants, no tenant scope filter).
        for (name, hash) in wasm_reg.all_builtins() {
            entries.push(make_entry("builtin", name, hash));
        }

        entries
    };

    let total = modules.len();
    Ok(Json(serde_json::json!({
        "modules": modules,
        "total": total,
    })))
}

/// GET /observe/wasm/invocations — query WASM invocation history.
#[instrument(skip_all, fields(otel.name = "GET /observe/wasm/invocations"))]
pub async fn handle_list_wasm_invocations(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(params): Query<InvocationQueryParams>,
) -> Result<Json<WasmInvocationResponse>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_wasm", "WasmModule")?;
    let limit = params.limit.unwrap_or(100).min(10_000);

    let mut all_filtered: Vec<serde_json::Value> = Vec::new();
    let store = state
        .metadata_store_for_tenant(authenticated.tenant().as_str())
        .await;
    for store in store.iter() {
        match store.load_recent_wasm_invocations(limit as i64).await {
            Ok(rows) => {
                let filtered: Vec<serde_json::Value> = rows
                    .into_iter()
                    .filter(|e| {
                        if let Some(ref mn) = params.module_name
                            && e.module_name != *mn
                        {
                            return false;
                        }
                        if let Some(s) = params.success
                            && e.success != s
                        {
                            return false;
                        }
                        true
                    })
                    .map(|e| serde_json::to_value(&e).unwrap_or_default())
                    .collect();
                all_filtered.extend(filtered);
            }
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), "failed to query WASM invocations");
            }
        }
    }

    let total = all_filtered.len();
    Ok(Json(WasmInvocationResponse {
        invocations: all_filtered,
        total,
    }))
}
