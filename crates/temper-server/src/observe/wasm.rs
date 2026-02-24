//! WASM module management endpoints.
//!
//! Upload, download, delete, and list WASM integration modules.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Serialize;

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

/// Entry in the module list response.
#[derive(Serialize)]
pub struct WasmModuleListEntry {
    /// Module name.
    pub module_name: String,
    /// SHA-256 hash of the module bytes.
    pub sha256_hash: String,
}

/// POST /observe/wasm/modules/{module_name} — upload a WASM binary.
pub async fn upload_wasm_module(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(module_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<WasmModuleUploadResponse>, (StatusCode, String)> {
    let tenant = extract_tenant(&headers, &state);

    // TigerStyle: pre-assertion on module size (10 MB budget)
    if body.len() > temper_wasm::types::MAX_MODULE_SIZE {
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
    let hash = state
        .wasm_engine
        .compile_and_cache(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("WASM compilation failed: {e}")))?;

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

/// DELETE /observe/wasm/modules/{module_name} — remove a module.
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

/// GET /observe/wasm/modules — list all modules for tenant.
pub async fn list_wasm_modules(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let tenant = extract_tenant(&headers, &state);

    let modules: Vec<WasmModuleListEntry> = {
        let wasm_reg = state.wasm_module_registry.read().unwrap();
        wasm_reg
            .modules_for_tenant(&tenant)
            .into_iter()
            .map(|(name, hash)| WasmModuleListEntry {
                module_name: name.to_string(),
                sha256_hash: hash.to_string(),
            })
            .collect()
    };

    let total = modules.len();
    Json(serde_json::json!({
        "tenant": tenant.to_string(),
        "modules": modules,
        "total": total,
    }))
}
