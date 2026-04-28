//! Object storage for durable blob bytes.
//!
//! New blob writes go through this boundary. The Turso `blobs` table remains a
//! legacy read fallback for installations that already wrote blob bytes there.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, HOST, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;

use crate::state::ServerState;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_BLOB_IO_MAX_CONCURRENCY: usize = 32;
pub(crate) const DEFAULT_BLOB_BUCKET: &str = "temper-fs";
pub(crate) const WASM_ARTIFACT_PREFIX: &str = "wasm-modules/";

pub(crate) fn wasm_artifact_key(sha256_hash: &str) -> String {
    format!("{WASM_ARTIFACT_PREFIX}{sha256_hash}")
}

#[derive(Clone, Debug)]
pub(crate) struct BlobStore {
    backend: BlobStoreBackend,
}

#[derive(Clone, Debug)]
enum BlobStoreBackend {
    LocalFs { root: PathBuf },
    S3(S3BlobStore),
}

#[derive(Clone, Debug)]
struct S3BlobStore {
    endpoint: String,
    bucket: String,
    access_key: Option<String>,
    secret_key: Option<String>,
    client: reqwest::Client,
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

impl BlobStore {
    pub(crate) fn local_fs(root: impl Into<PathBuf>) -> Self {
        Self {
            backend: BlobStoreBackend::LocalFs { root: root.into() },
        }
    }

    pub(crate) fn s3(
        endpoint: impl Into<String>,
        bucket: impl Into<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
    ) -> Self {
        Self {
            backend: BlobStoreBackend::S3(S3BlobStore {
                endpoint: endpoint.into().trim_end_matches('/').to_string(),
                bucket: bucket.into().trim_matches('/').to_string(),
                access_key,
                secret_key,
                client: reqwest::Client::new(),
            }),
        }
    }

    pub(crate) async fn put_if_absent(
        &self,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let queued_at = Instant::now();
        let _permit = blob_io_semaphore()
            .acquire()
            .await
            .expect("blob semaphore closed"); // ci-ok: process-global and never closed
        let wait_duration = queued_at.elapsed();
        crate::runtime_metrics::record_blob_io_wait_duration(wait_duration, "put");
        if wait_duration.as_millis() > 0 {
            tracing::info!(path = %key, wait_ms = wait_duration.as_millis() as u64, "blob put queued");
        }
        if ttl.is_some() {
            tracing::debug!(
                path = %key,
                ttl_seconds = ttl.map(|duration| duration.as_secs()),
                "object blob store write received TTL; retention is delegated to the object store"
            );
        }

        match &self.backend {
            BlobStoreBackend::LocalFs { root } => put_local_blob(root, key, body).await,
            BlobStoreBackend::S3(store) => store.put_if_absent(key, body).await,
        }
    }

    pub(crate) async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let queued_at = Instant::now();
        let _permit = blob_io_semaphore()
            .acquire()
            .await
            .expect("blob semaphore closed"); // ci-ok: process-global and never closed
        let wait_duration = queued_at.elapsed();
        crate::runtime_metrics::record_blob_io_wait_duration(wait_duration, "get");
        if wait_duration.as_millis() > 0 {
            tracing::info!(path = %key, wait_ms = wait_duration.as_millis() as u64, "blob get queued");
        }

        match &self.backend {
            BlobStoreBackend::LocalFs { root } => get_local_blob(root, key).await,
            BlobStoreBackend::S3(store) => store.get(key).await,
        }
    }
}

impl ServerState {
    pub(crate) fn blob_store_for_tenant(&self, tenant: &TenantId) -> Result<BlobStore, String> {
        if let Some(vault) = self.secrets_vault.as_ref()
            && let Some(endpoint) = vault.get_secret(tenant.as_str(), "blob_endpoint")
            && !endpoint.trim().is_empty()
        {
            if is_local_internal_blob_endpoint(&endpoint) {
                return self.local_blob_store().ok_or_else(|| {
                    "internal DB-backed blob endpoint is disabled; set TEMPER_LOCAL_BLOB_DIR or configure BLOB_ENDPOINT for R2/S3"
                        .to_string()
                });
            }

            let bucket = vault
                .get_secret(tenant.as_str(), "blob_bucket")
                .unwrap_or_else(|| DEFAULT_BLOB_BUCKET.to_string());
            let access_key = vault.get_secret(tenant.as_str(), "blob_access_key");
            let secret_key = vault.get_secret(tenant.as_str(), "blob_secret_key");
            return Ok(BlobStore::s3(endpoint, bucket, access_key, secret_key));
        }

        self.local_blob_store().ok_or_else(|| {
            "blob object store is not configured; set BLOB_ENDPOINT/BLOB_BUCKET/BLOB_ACCESS_KEY/BLOB_SECRET_KEY or TEMPER_LOCAL_BLOB_DIR"
                .to_string()
        })
    }

    pub(crate) async fn put_blob_object(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let store = self.blob_store_for_tenant(tenant)?;
        store.put_if_absent(key, body, ttl).await
    }

    pub(crate) async fn get_blob_with_legacy_fallback(
        &self,
        tenant: &TenantId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        match self.blob_store_for_tenant(tenant) {
            Ok(store) => match store.get(key).await {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%key, %error, "object blob store read failed; trying legacy DB blob fallback");
                }
            },
            Err(error) => {
                tracing::debug!(%key, %error, "object blob store unavailable; trying legacy DB blob fallback");
            }
        }

        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(None);
        };
        store
            .get_blob(key)
            .await
            .map_err(|e| format!("legacy DB blob read failed for '{key}': {e}"))
    }

    fn local_blob_store(&self) -> Option<BlobStore> {
        if let Ok(root) = std::env::var("TEMPER_LOCAL_BLOB_DIR")
            && !root.trim().is_empty()
        {
            return Some(BlobStore::local_fs(root));
        }
        if !self.data_dir.as_os_str().is_empty() {
            return Some(BlobStore::local_fs(self.data_dir.join("blobs")));
        }
        None
    }
}

impl S3BlobStore {
    async fn put_if_absent(&self, key: &str, body: &[u8]) -> Result<(), String> {
        if self.exists(key).await? {
            return Ok(());
        }

        let url = self.object_url(key);
        let mut request = self.client.put(&url).body(body.to_vec());
        let headers = self.signed_headers(Method::PUT, &url)?;
        for (header_name, header_value) in &headers {
            request = request.header(header_name, header_value);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("blob PUT request failed for '{key}': {e}"))?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(format!(
            "blob PUT failed for '{key}' with HTTP {}",
            response.status()
        ))
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let url = self.object_url(key);
        let mut request = self.client.get(&url);
        let headers = self.signed_headers(Method::GET, &url)?;
        for (header_name, header_value) in &headers {
            request = request.header(header_name, header_value);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("blob GET request failed for '{key}': {e}"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(format!(
                "blob GET failed for '{key}' with HTTP {}",
                response.status()
            ));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("blob GET body read failed for '{key}': {e}"))?;
        Ok(Some(bytes.to_vec()))
    }

    async fn exists(&self, key: &str) -> Result<bool, String> {
        let url = self.object_url(key);
        let mut request = self.client.head(&url);
        let headers = self.signed_headers(Method::HEAD, &url)?;
        for (header_name, header_value) in &headers {
            request = request.header(header_name, header_value);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("blob HEAD request failed for '{key}': {e}"))?;
        match response.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            status if status.is_success() => Ok(true),
            status => Err(format!("blob HEAD failed for '{key}' with HTTP {status}")),
        }
    }

    fn object_url(&self, key: &str) -> String {
        format!(
            "{}/{}/{}",
            self.endpoint,
            self.bucket,
            key.trim_start_matches('/')
        )
    }

    fn signed_headers(&self, method: Method, url: &str) -> Result<HeaderMap, String> {
        let mut headers = HeaderMap::new();
        let Some(access_key) = self.access_key.as_deref() else {
            return Ok(headers);
        };
        let Some(secret_key) = self.secret_key.as_deref() else {
            return Ok(headers);
        };

        let datetime = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let date = &datetime[..8];
        let (host, path) = parse_url_host_path(url);
        let canonical_uri = uri_encode_path(path);
        let payload_hash = "UNSIGNED-PAYLOAD";
        let region = "auto";
        let service = "s3";
        let scope = format!("{date}/{region}/{service}/aws4_request");
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{datetime}\n");
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            method.as_str()
        );
        let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{canonical_request_hash}");
        let signing_key = derive_signing_key(secret_key, date, region, service);
        let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{scope},SignedHeaders={signed_headers},Signature={signature}"
        );

        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&authorization)
                .map_err(|e| format!("invalid blob authorization header: {e}"))?,
        );
        headers.insert(
            "x-amz-date",
            HeaderValue::from_str(&datetime)
                .map_err(|e| format!("invalid x-amz-date header: {e}"))?,
        );
        headers.insert(
            "x-amz-content-sha256",
            HeaderValue::from_static(payload_hash),
        );
        headers.insert(
            HOST,
            HeaderValue::from_str(host).map_err(|e| format!("invalid blob host header: {e}"))?,
        );
        Ok(headers)
    }
}

async fn put_local_blob(root: &Path, key: &str, body: &[u8]) -> Result<(), String> {
    let path = local_blob_path(root, key)?;
    if tokio::fs::try_exists(&path)
        .await
        .map_err(|e| format!("failed to check local blob '{}': {e}", path.display()))?
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            format!(
                "failed to create local blob dir '{}': {e}",
                parent.display()
            )
        })?;
    }
    tokio::fs::write(&path, body)
        .await
        .map_err(|e| format!("failed to write local blob '{}': {e}", path.display()))
}

async fn get_local_blob(root: &Path, key: &str) -> Result<Option<Vec<u8>>, String> {
    let path = local_blob_path(root, key)?;
    if !tokio::fs::try_exists(&path)
        .await
        .map_err(|e| format!("failed to check local blob '{}': {e}", path.display()))?
    {
        return Ok(None);
    }
    tokio::fs::read(&path)
        .await
        .map(Some)
        .map_err(|e| format!("failed to read local blob '{}': {e}", path.display()))
}

fn local_blob_path(root: &Path, key: &str) -> Result<PathBuf, String> {
    let mut path = root.to_path_buf();
    let mut saw_component = false;
    for component in key.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\\')
            || component.starts_with(std::path::MAIN_SEPARATOR)
        {
            return Err(format!("invalid blob key '{key}'"));
        }
        saw_component = true;
        path.push(component);
    }
    if !saw_component {
        return Err("invalid empty blob key".to_string());
    }
    Ok(path)
}

pub(crate) fn is_local_internal_blob_endpoint(endpoint: &str) -> bool {
    let normalized = endpoint.trim_end_matches('/');
    (normalized.starts_with("http://127.0.0.1:") || normalized.starts_with("http://localhost:"))
        && normalized.contains("/_internal/blobs")
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

fn derive_signing_key(secret_key: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn parse_url_host_path(url: &str) -> (&str, &str) {
    let after_scheme = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };
    if let Some(slash) = after_scheme.find('/') {
        (&after_scheme[..slash], &after_scheme[slash..])
    } else {
        (after_scheme, "/")
    }
}

fn uri_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 16);
    for byte in path.bytes() {
        match byte {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(byte as char),
            _ => {
                out.push('%');
                out.push(b"0123456789ABCDEF"[(byte >> 4) as usize] as char);
                out.push(b"0123456789ABCDEF"[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_blob_store_round_trips_without_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::local_fs(dir.path());

        store
            .put_if_absent("wasm-modules/hash-a", b"hello", None)
            .await
            .expect("put");

        let bytes = store
            .get("wasm-modules/hash-a")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(bytes, b"hello");
        assert!(
            dir.path().join("wasm-modules").join("hash-a").is_file(),
            "blob is stored in the filesystem object store"
        );
    }

    #[tokio::test]
    async fn local_blob_store_rejects_path_traversal_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::local_fs(dir.path());

        let err = store
            .put_if_absent("../escape", b"nope", None)
            .await
            .expect_err("path traversal rejected");
        assert!(err.contains("invalid blob key"));
    }
}
