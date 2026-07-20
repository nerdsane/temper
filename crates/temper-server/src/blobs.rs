//! Internal blob storage endpoint and field-overflow helpers.
//!
//! New writes go through the Temper object-store boundary. Turso DB blobs are
//! read only as a legacy fallback for data written by older releases.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use temper_runtime::tenant::TenantId;

use crate::blob_store::BlobStore;
use crate::state::ServerState;

pub(crate) const FIELD_OVERFLOW_BLOB_PREFIX: &str = "field-overflow/sha256/";
pub(crate) const FIELD_OVERFLOW_REF_KEY: &str = "__temper_blob_ref";
pub(crate) const FIELD_OVERFLOW_SIZE_KEY: &str = "__temper_blob_size";
pub(crate) const FIELD_OVERFLOW_ENCODING_KEY: &str = "__temper_blob_encoding";

#[derive(Debug, Clone)]
pub struct OverflowBlobWrite {
    pub key: String,
    pub body: Vec<u8>,
    /// Optional per-field TTL carried from IOA spec
    /// (`overflow_ttl_seconds`, ADR-0047). `None` means permanent.
    pub ttl_seconds: Option<u64>,
}

#[cfg(test)]
pub(crate) async fn get_blob_bytes(
    store: &BlobStore,
    key: &str,
) -> Result<Option<Vec<u8>>, String> {
    store.get(key).await
}

pub(crate) async fn put_overflow_blobs(
    store: &BlobStore,
    blobs: &[OverflowBlobWrite],
) -> Result<(), String> {
    for blob in blobs {
        let ttl = blob.ttl_seconds.map(std::time::Duration::from_secs);
        put_blob_bytes_with_ttl(store, &blob.key, &blob.body, ttl).await?;
    }
    Ok(())
}

/// TTL-aware variant of `put_blob_bytes` (ADR-0047).
pub(crate) async fn put_blob_bytes_with_ttl(
    store: &BlobStore,
    key: &str,
    body: &[u8],
    ttl: Option<std::time::Duration>,
) -> Result<(), String> {
    store.put_if_absent(key, body, ttl).await
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

enum BlobReadSource<'a> {
    #[cfg(test)]
    Store(&'a BlobStore),
    Tenant {
        state: &'a ServerState,
        tenant: &'a TenantId,
    },
}

async fn read_blob_ref_bytes(
    source: &BlobReadSource<'_>,
    key: &str,
) -> Result<Option<Vec<u8>>, String> {
    match source {
        #[cfg(test)]
        BlobReadSource::Store(store) => get_blob_bytes(store, key).await,
        BlobReadSource::Tenant { state, tenant } => {
            state.get_blob_with_legacy_fallback(tenant, key).await
        }
    }
}

#[cfg(test)]
pub(crate) async fn hydrate_blob_refs_in_value(store: &BlobStore, value: &mut Value) {
    // OData callers want full inline hydration regardless of size.
    let _deferred = hydrate_blob_refs_in_value_with_ceiling(store, value, usize::MAX).await;
}

/// Hydrate blob refs in `value` below `max_inline_bytes` in place; return a
/// `BTreeMap` of blob keys to bytes for refs at or above the ceiling (the
/// "deferred" set). Callers that hand `value` off to a WASM guest forward
/// the deferred map as `blob_cache` so guests can resolve oversize fields
/// via `host_read_field_stream`. See ADR-0046.
#[cfg(test)]
pub(crate) async fn hydrate_blob_refs_in_value_with_ceiling(
    store: &BlobStore,
    value: &mut Value,
    max_inline_bytes: usize,
) -> std::collections::BTreeMap<String, Vec<u8>> {
    hydrate_blob_refs_with_source(&BlobReadSource::Store(store), value, max_inline_bytes).await
}

async fn hydrate_blob_refs_with_source(
    source: &BlobReadSource<'_>,
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
        if let Some(size) = declared_size
            && size > max_inline_bytes
        {
            match read_blob_ref_bytes(source, &key).await {
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

        match read_blob_ref_bytes(source, &key).await {
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
    let _deferred =
        hydrate_blob_refs_with_source(&BlobReadSource::Tenant { state, tenant }, value, usize::MAX)
            .await;
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
    hydrate_blob_refs_with_source(
        &BlobReadSource::Tenant { state, tenant },
        value,
        max_inline_bytes,
    )
    .await
}

/// `PUT /_internal/blobs/{*path}` — store a blob.
pub async fn put_blob(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let tenant = TenantId::new("default");

    match state.put_blob_object(&tenant, &path, &body, None).await {
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
    let tenant = TenantId::new("default");

    match state.get_blob_with_legacy_fallback(&tenant, &path).await {
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

#[cfg(test)]
mod tests;
