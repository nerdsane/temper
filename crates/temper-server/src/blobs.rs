//! Internal blob storage endpoint for TemperFS.
//!
//! Provides `PUT/GET /_internal/blobs/{*path}` backed by Turso.
//! The blob_adapter WASM module uploads/downloads through these endpoints
//! when no external blob storage (R2/S3) is configured.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Instant;
use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;

use crate::state::ServerState;

const DEFAULT_BLOB_IO_MAX_CONCURRENCY: usize = 32;
pub(crate) const FIELD_OVERFLOW_BLOB_PREFIX: &str = "field-overflow/sha256/";
pub(crate) const FIELD_OVERFLOW_REF_KEY: &str = "__temper_blob_ref";
pub(crate) const FIELD_OVERFLOW_SIZE_KEY: &str = "__temper_blob_size";
pub(crate) const FIELD_OVERFLOW_ENCODING_KEY: &str = "__temper_blob_encoding";

#[derive(Debug, Clone)]
pub struct OverflowBlobWrite {
    pub key: String,
    pub body: Vec<u8>,
}

fn blob_io_semaphore() -> &'static Semaphore {
    static SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
    SEMAPHORE.get_or_init(|| {
        let limit = std::env::var("TEMPER_BLOB_IO_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_BLOB_IO_MAX_CONCURRENCY);
        Semaphore::new(limit)
    })
}

pub(crate) async fn put_blob_bytes(
    store: &temper_store_turso::TursoEventStore,
    key: &str,
    body: &[u8],
) -> Result<(), String> {
    let queued_at = Instant::now();
    let _permit = blob_io_semaphore()
        .acquire()
        .await
        .expect("blob semaphore closed"); // ci-ok: semaphore is process-global and never closed
    let wait_duration = queued_at.elapsed();
    let wait_ms = wait_duration.as_millis() as u64;
    crate::runtime_metrics::record_blob_io_wait_duration(wait_duration, "put");
    if wait_ms > 0 {
        tracing::info!(path = %key, wait_ms, "blob put queued");
    }

    store.put_blob(key, body).await
}

pub(crate) async fn get_blob_bytes(
    store: &temper_store_turso::TursoEventStore,
    key: &str,
) -> Result<Option<Vec<u8>>, String> {
    let queued_at = Instant::now();
    let _permit = blob_io_semaphore()
        .acquire()
        .await
        .expect("blob semaphore closed"); // ci-ok: semaphore is process-global and never closed
    let wait_duration = queued_at.elapsed();
    let wait_ms = wait_duration.as_millis() as u64;
    crate::runtime_metrics::record_blob_io_wait_duration(wait_duration, "get");
    if wait_ms > 0 {
        tracing::info!(path = %key, wait_ms, "blob get queued");
    }

    store.get_blob(key).await
}

pub(crate) async fn put_overflow_blobs(
    store: &temper_store_turso::TursoEventStore,
    blobs: &[OverflowBlobWrite],
) -> Result<(), String> {
    for blob in blobs {
        put_blob_bytes(store, &blob.key, &blob.body).await?;
    }
    Ok(())
}

pub(crate) fn blob_ref_value(key: &str, size_bytes: usize) -> Value {
    serde_json::json!({
        FIELD_OVERFLOW_REF_KEY: key,
        FIELD_OVERFLOW_SIZE_KEY: size_bytes,
        FIELD_OVERFLOW_ENCODING_KEY: "json",
    })
}

fn blob_ref_key(value: &Value) -> Option<&str> {
    value
        .as_object()
        .and_then(|obj| obj.get(FIELD_OVERFLOW_REF_KEY))
        .and_then(|value| value.as_str())
}

fn collect_blob_ref_pointers(value: &Value, pointer: &str, out: &mut Vec<String>) {
    if blob_ref_key(value).is_some() {
        out.push(pointer.to_string());
        return;
    }

    match value {
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                let child_pointer = format!("{pointer}/{index}");
                collect_blob_ref_pointers(child, &child_pointer, out);
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                let child_pointer = format!("{pointer}/{escaped}");
                collect_blob_ref_pointers(child, &child_pointer, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

pub(crate) async fn hydrate_blob_refs_in_value(
    store: &temper_store_turso::TursoEventStore,
    value: &mut Value,
) {
    // OData callers want full inline hydration regardless of size.
    let _deferred = hydrate_blob_refs_in_value_with_ceiling(store, value, usize::MAX).await;
}

/// Hydrate blob refs in `value` below `max_inline_bytes` in place; return a
/// `BTreeMap` of blob keys to bytes for refs at or above the ceiling (the
/// "deferred" set). Callers that hand `value` off to a WASM guest forward
/// the deferred map as `blob_cache` so guests can resolve oversize fields
/// via `host_read_field_stream`. See ADR-0046.
pub(crate) async fn hydrate_blob_refs_in_value_with_ceiling(
    store: &temper_store_turso::TursoEventStore,
    value: &mut Value,
    max_inline_bytes: usize,
) -> std::collections::BTreeMap<String, Vec<u8>> {
    use std::collections::BTreeMap;

    let mut deferred_blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut pointers = Vec::new();
    collect_blob_ref_pointers(value, "", &mut pointers);
    // DST: deterministic fetch order across runs with the same ref set.
    pointers.sort();

    for pointer in pointers {
        let (key, declared_size) = {
            let slot = if pointer.is_empty() {
                Some(&*value)
            } else {
                value.pointer(&pointer)
            };
            let Some(slot) = slot else {
                continue;
            };
            let key = slot
                .as_object()
                .and_then(|obj| obj.get(FIELD_OVERFLOW_REF_KEY))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let size = slot
                .as_object()
                .and_then(|obj| obj.get(FIELD_OVERFLOW_SIZE_KEY))
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            match key {
                Some(k) => (k, size),
                None => continue,
            }
        };

        // Fast-path: if the envelope declares a size above the ceiling, don't
        // fetch inline — just fetch once into the deferred map.
        if let Some(size) = declared_size {
            if size > max_inline_bytes {
                match get_blob_bytes(store, &key).await {
                    Ok(Some(bytes)) => {
                        deferred_blobs.insert(key, bytes);
                    }
                    Ok(None) => {
                        tracing::warn!(%key, "deferred field-overflow blob missing");
                    }
                    Err(error) => {
                        tracing::warn!(%key, %error, "failed to fetch deferred field-overflow blob");
                    }
                }
                continue;
            }
        }

        match get_blob_bytes(store, &key).await {
            Ok(Some(bytes)) => {
                // Post-fetch size check in case the envelope lied (missing size key).
                if bytes.len() > max_inline_bytes {
                    deferred_blobs.insert(key, bytes);
                    continue;
                }
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(restored) => {
                        if pointer.is_empty() {
                            *value = restored;
                        } else if let Some(slot) = value.pointer_mut(&pointer) {
                            *slot = restored;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%key, %error, "failed to decode hydrated field-overflow blob");
                    }
                }
            }
            Ok(None) => {
                tracing::warn!(%key, "field-overflow blob missing during hydration");
            }
            Err(error) => {
                tracing::warn!(%key, %error, "failed to hydrate field-overflow blob");
            }
        }
    }

    deferred_blobs
}

pub(crate) async fn hydrate_blob_refs_for_tenant(
    state: &ServerState,
    tenant: &TenantId,
    value: &mut Value,
) {
    let Some(store) = state.persistent_store_for_tenant(tenant.as_str()).await else {
        return;
    };
    hydrate_blob_refs_in_value(&store, value).await;
}

/// Tenant-scoped variant of `hydrate_blob_refs_in_value_with_ceiling`.
///
/// Returns an empty map if no Turso store is configured for the tenant — in
/// that case, the entity state stays untouched, which is consistent with
/// `hydrate_blob_refs_for_tenant`'s no-op behavior.
pub(crate) async fn hydrate_blob_refs_for_tenant_with_ceiling(
    state: &ServerState,
    tenant: &TenantId,
    value: &mut Value,
    max_inline_bytes: usize,
) -> std::collections::BTreeMap<String, Vec<u8>> {
    let Some(store) = state.persistent_store_for_tenant(tenant.as_str()).await else {
        return std::collections::BTreeMap::new();
    };
    hydrate_blob_refs_in_value_with_ceiling(&store, value, max_inline_bytes).await
}

/// `PUT /_internal/blobs/{*path}` — store a blob.
pub async fn put_blob(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let Some(store) = state.platform_persistent_store().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Blob storage requires Turso".to_string(),
        )
            .into_response();
    };

    match put_blob_bytes(&store, &path, &body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %path, "blob put failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

/// `GET /_internal/blobs/{*path}` — retrieve a blob.
pub async fn get_blob(
    State(state): State<ServerState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let Some(store) = state.platform_persistent_store().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Blob storage requires Turso".to_string(),
        )
            .into_response();
    };

    match get_blob_bytes(&store, &path).await {
        Ok(Some(data)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %path, "blob get failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}
