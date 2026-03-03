//! WASM module management endpoints.
//!
//! Upload, download, delete, and list WASM integration modules.

use axum::extract::Path;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::{Deserialize, Serialize};

use tracing::instrument;

use crate::dispatch::extract_tenant;
use crate::state::ServerState;

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
#[instrument(skip_all, fields(module_name, otel.name = "POST /api/wasm/modules/{module_name}"))]
pub async fn upload_wasm_module(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(module_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<WasmModuleUploadResponse>, (StatusCode, String)> {
    let tenant = extract_tenant(&headers, &state);

    // TigerStyle: pre-assertion on module size (10 MB budget)
    if body.len() > temper_wasm::types::MAX_MODULE_SIZE {
        tracing::warn!(size = body.len(), "WASM module too large");
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "WASM module too large: {} bytes (max {})",
                body.len(),
                temper_wasm::types::MAX_MODULE_SIZE
            ),
        ));
    }

    // Compile and cache
    let hash = state.wasm_engine.compile_and_cache(&body).map_err(|e| {
        tracing::warn!(error = %e, "WASM compilation failed");
        (
            StatusCode::BAD_REQUEST,
            format!("WASM compilation failed: {e}"),
        )
    })?;

    // Register in module registry
    {
        let mut wasm_reg = state.wasm_module_registry.write().unwrap();
        wasm_reg.register(&tenant, &module_name, &hash);
    }

    // Persist to store (best-effort)
    if let Err(e) = state
        .upsert_wasm_module(tenant.as_str(), &module_name, &body, &hash)
        .await
    {
        tracing::warn!(error = %e, "failed to persist WASM module (in-memory registration succeeded)");
    }

    let size_bytes = body.len();
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

/// GET /observe/wasm/modules/{module_name} — module info.
#[instrument(skip_all, fields(module_name, otel.name = "GET /observe/wasm/modules/{module_name}"))]
pub async fn get_wasm_module_info(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(module_name): Path<String>,
) -> Result<Json<WasmModuleInfoResponse>, (StatusCode, String)> {
    let tenant = extract_tenant(&headers, &state);

    let hash = {
        let wasm_reg = state.wasm_module_registry.read().unwrap();
        wasm_reg
            .get_hash(&tenant, &module_name)
            .map(|s| s.to_string())
    };

    let Some(hash) = hash else {
        tracing::warn!("WASM module not found");
        return Err((
            StatusCode::NOT_FOUND,
            format!("WASM module '{module_name}' not found for tenant '{tenant}'"),
        ));
    };

    let cached = state.wasm_engine.is_cached(&hash);

    Ok(Json(WasmModuleInfoResponse {
        module_name,
        sha256_hash: hash,
        cached,
    }))
}

/// DELETE /api/wasm/modules/{module_name} — remove a module.
#[instrument(skip_all, fields(module_name, otel.name = "DELETE /api/wasm/modules/{module_name}"))]
pub async fn delete_wasm_module(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(module_name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let tenant = extract_tenant(&headers, &state);

    // Get hash before removing from registry (for cache eviction)
    let hash = {
        let wasm_reg = state.wasm_module_registry.read().unwrap();
        wasm_reg
            .get_hash(&tenant, &module_name)
            .map(|s| s.to_string())
    };

    // Remove from registry
    let removed = {
        let mut wasm_reg = state.wasm_module_registry.write().unwrap();
        wasm_reg.remove(&tenant, &module_name)
    };

    if !removed {
        tracing::warn!("WASM module not found for deletion");
        return Err((
            StatusCode::NOT_FOUND,
            format!("WASM module '{module_name}' not found for tenant '{tenant}'"),
        ));
    }

    // Evict from engine cache
    if let Some(hash) = hash {
        state.wasm_engine.evict(&hash);
    }

    // Delete from persistence (best-effort)
    if let Err(e) = state
        .delete_wasm_module(tenant.as_str(), &module_name)
        .await
    {
        tracing::warn!(error = %e, "failed to delete WASM module from persistence");
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

/// GET /observe/wasm/modules — list all modules for tenant (with stats).
#[instrument(skip_all, fields(otel.name = "GET /observe/wasm/modules"))]
pub async fn list_wasm_modules(
    State(state): State<ServerState>,
    _headers: HeaderMap,
) -> Json<serde_json::Value> {
    // Observe endpoints are cross-tenant — return all modules across all tenants.

    // Collect invocation stats — Turso is single source of truth but we
    // aggregate from load_recent_wasm_invocations; for module list we
    // just use an empty stats map if Turso is unavailable.
    let invocation_stats: std::collections::BTreeMap<String, (usize, usize, Option<String>)> = {
        let mut stats: std::collections::BTreeMap<String, (usize, usize, Option<String>)> =
            std::collections::BTreeMap::new();
        if let Some(turso) = state.turso_opt()
            && let Ok(rows) = turso.load_recent_wasm_invocations(10_000).await
        {
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
        stats
    };

    let modules: Vec<WasmModuleListEntry> = {
        let wasm_reg = state.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
        wasm_reg
            .all_modules()
            .into_iter()
            .map(|(tenant, name, hash)| {
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
            })
            .collect()
    };

    let total = modules.len();
    Json(serde_json::json!({
        "modules": modules,
        "total": total,
    }))
}

/// GET /observe/wasm/invocations — query WASM invocation history from Turso.
#[instrument(skip_all, fields(otel.name = "GET /observe/wasm/invocations"))]
pub async fn list_wasm_invocations(
    State(state): State<ServerState>,
    Query(params): Query<InvocationQueryParams>,
) -> Json<WasmInvocationResponse> {
    let limit = params.limit.unwrap_or(100).min(10_000);

    // Query Turso directly (single source of truth).
    if let Some(turso) = state.turso_opt() {
        match turso.load_recent_wasm_invocations(limit as i64).await {
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
                let total = filtered.len();
                return Json(WasmInvocationResponse {
                    invocations: filtered,
                    total,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query WASM invocations from Turso");
            }
        }
    }

    // Fallback: empty response when no Turso configured.
    Json(WasmInvocationResponse {
        invocations: Vec::new(),
        total: 0,
    })
}
