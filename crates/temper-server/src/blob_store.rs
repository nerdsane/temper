//! Durable blob storage, with a Turso legacy-read fallback.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use tracing::Instrument;

use crate::aws_sigv4;
use crate::blob_transport_observability::{
    BlobTransportError, BlobTransportFinish, blob_transport_span, finish_blob_transport,
};
mod endpoint;
mod keys;
mod limits;
mod local;
mod raw_ingest;
mod receipt;
mod state;
mod streaming;

pub(crate) use endpoint::{LocalInternalBlobEndpoint, is_local_internal_blob_endpoint};
pub(crate) use keys::{DEFAULT_BLOB_BUCKET, hex_lower, wasm_artifact_key};
use limits::{BLOB_BUFFERED_OPERATION_TIMEOUT, BLOB_IO_QUEUE_TIMEOUT, blob_io_semaphore};
use local::{get_local_blob_observed, local_blob_path, put_local_blob_observed};
pub use raw_ingest::BlobByteStream;
#[cfg(test)]
pub(crate) use raw_ingest::BlobIngestProgressPolicy;
pub(crate) use raw_ingest::{
    BlobIngestAdmissionError, BlobIngestBudget, BlobStageError, MAX_RAW_BLOB_BYTES,
};
pub(crate) use receipt::CommittedStreamReceiptV1;
pub(crate) use streaming::BlobReadBounded;
pub use streaming::{BlobObjectStream, BlobStreamRead, decode_json_base64_stream};

#[derive(Clone, Debug)]
pub(crate) struct BlobStore {
    backend: BlobStoreBackend,
    staging_root: PathBuf,
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
    key_prefix: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    client: reqwest::Client,
}

impl BlobStore {
    pub(crate) fn local_fs(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            staging_root: root.join(".ingest-staging"),
            backend: BlobStoreBackend::LocalFs { root },
        }
    }

    pub(crate) fn s3(
        endpoint: impl Into<String>,
        bucket: impl Into<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
        staging_root: impl Into<PathBuf>,
        key_prefix: Option<String>,
    ) -> Self {
        Self {
            staging_root: staging_root.into(),
            backend: BlobStoreBackend::S3(S3BlobStore {
                endpoint: endpoint.into().trim_end_matches('/').to_string(),
                bucket: bucket.into().trim_matches('/').to_string(),
                key_prefix,
                access_key,
                secret_key,
                client: reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .build()
                    .expect("static blob HTTP client configuration must be valid"), // ci-ok: static reqwest builder options
            }),
        }
    }

    pub(crate) async fn put_if_absent(
        &self,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let queued_at = Instant::now(); // determinism-ok: production blob I/O queue metric only
        let _permit =
            tokio::time::timeout(BLOB_IO_QUEUE_TIMEOUT, blob_io_semaphore().acquire_owned())
                .await
                .map_err(|_| "blob put queue deadline exceeded".to_string())?
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

        tokio::time::timeout(BLOB_BUFFERED_OPERATION_TIMEOUT, async {
            match &self.backend {
                BlobStoreBackend::LocalFs { root } => {
                    put_local_blob_observed(root, key, body, "put").await
                }
                BlobStoreBackend::S3(store) => store.put_if_absent(key, body).await,
            }
        })
        .await
        .map_err(|_| format!("blob put timed out for '{key}'"))?
    }

    /// Write bytes to a content-addressed key.
    ///
    /// Remote object stores use a direct `PUT` because the key is already
    /// derived from the payload hash; avoiding a preflight existence check
    /// saves a round trip on the File `$value` write path.
    pub(crate) async fn put_content_addressed(
        &self,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let queued_at = Instant::now(); // determinism-ok: production blob I/O queue metric only
        let _permit =
            tokio::time::timeout(BLOB_IO_QUEUE_TIMEOUT, blob_io_semaphore().acquire_owned())
                .await
                .map_err(|_| "content-addressed blob put queue deadline exceeded".to_string())?
                .expect("blob semaphore closed"); // ci-ok: process-global and never closed
        let wait_duration = queued_at.elapsed();
        crate::runtime_metrics::record_blob_io_wait_duration(wait_duration, "put_content");
        if wait_duration.as_millis() > 0 {
            tracing::info!(path = %key, wait_ms = wait_duration.as_millis() as u64, "content-addressed blob put queued");
        }
        if ttl.is_some() {
            tracing::debug!(
                path = %key,
                ttl_seconds = ttl.map(|duration| duration.as_secs()),
                "content-addressed blob write received TTL; retention is delegated to the object store"
            );
        }

        tokio::time::timeout(BLOB_BUFFERED_OPERATION_TIMEOUT, async {
            match &self.backend {
                BlobStoreBackend::LocalFs { root } => {
                    local::put_local_blob_replace_observed(root, key, body, "put_content").await
                }
                BlobStoreBackend::S3(store) => {
                    store.put_with_operation("put_content", key, body).await
                }
            }
        })
        .await
        .map_err(|_| format!("content-addressed blob put timed out for '{key}'"))?
    }

    pub(crate) async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let queued_at = Instant::now(); // determinism-ok: production blob I/O queue metric only
        let _permit =
            tokio::time::timeout(BLOB_IO_QUEUE_TIMEOUT, blob_io_semaphore().acquire_owned())
                .await
                .map_err(|_| "blob get queue deadline exceeded".to_string())?
                .expect("blob semaphore closed"); // ci-ok: process-global and never closed
        let wait_duration = queued_at.elapsed();
        crate::runtime_metrics::record_blob_io_wait_duration(wait_duration, "get");
        if wait_duration.as_millis() > 0 {
            tracing::info!(path = %key, wait_ms = wait_duration.as_millis() as u64, "blob get queued");
        }

        tokio::time::timeout(BLOB_BUFFERED_OPERATION_TIMEOUT, async {
            match &self.backend {
                BlobStoreBackend::LocalFs { root } => get_local_blob_observed(root, key).await,
                BlobStoreBackend::S3(store) => store.get(key).await,
            }
        })
        .await
        .map_err(|_| format!("blob get timed out for '{key}'"))?
    }
}

impl S3BlobStore {
    async fn put_if_absent(&self, key: &str, body: &[u8]) -> Result<(), String> {
        if self.exists(key).await? {
            return Ok(());
        }

        self.put_with_operation("put", key, body).await
    }

    async fn put_with_operation(
        &self,
        operation: &'static str,
        key: &str,
        body: &[u8],
    ) -> Result<(), String> {
        let request_bytes = body.len() as u64;
        let started_at = Instant::now(); // determinism-ok: production blob transport metric only
        let span = blob_transport_span(operation, "s3", request_bytes);
        let result = async {
            let url = self.object_url(key);
            let mut request = self.client.put(&url).body(body.to_vec());
            let headers = self
                .signed_headers(Method::PUT, &url)
                .map_err(BlobTransportError::message)?;
            for (header_name, header_value) in &headers {
                request = request.header(header_name, header_value);
            }

            let response = request.send().await.map_err(|e| {
                BlobTransportError::message(format!("blob PUT request failed for '{key}': {e}"))
            })?;
            let status = response.status();
            if status.is_success() {
                return Ok(status);
            }
            Err(BlobTransportError::status(
                format!("blob PUT failed for '{key}' with HTTP {status}"),
                status,
            ))
        }
        .instrument(span.clone())
        .await;

        match result {
            Ok(status) => {
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation,
                    backend: "s3",
                    outcome: "ok",
                    status: Some(status),
                    request_bytes,
                    response_bytes: 0,
                });
                Ok(())
            }
            Err(error) => {
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation,
                    backend: "s3",
                    outcome: "error",
                    status: error.status,
                    request_bytes,
                    response_bytes: 0,
                });
                Err(error.message)
            }
        }
    }

    async fn put_stream_with_operation(
        &self,
        operation: &'static str,
        key: &str,
        stream: BlobByteStream,
        content_len: u64,
    ) -> Result<(), String> {
        let started_at = Instant::now(); // determinism-ok: production blob transport metric only
        let span = blob_transport_span(operation, "s3", content_len);
        let result = async {
            let url = self.object_url(key);
            let mut request = self
                .client
                .put(&url)
                .header(reqwest::header::CONTENT_LENGTH, content_len)
                .timeout(raw_ingest::BLOB_BACKEND_OPERATION_TIMEOUT)
                .body(reqwest::Body::wrap_stream(stream));
            let headers = self
                .signed_headers(Method::PUT, &url)
                .map_err(BlobTransportError::message)?;
            for (header_name, header_value) in &headers {
                request = request.header(header_name, header_value);
            }

            let response = request.send().await.map_err(|error| {
                BlobTransportError::message(format!(
                    "streaming blob PUT request failed for '{key}': {error}"
                ))
            })?;
            let status = response.status();
            if status.is_success() {
                return Ok(status);
            }
            Err(BlobTransportError::status(
                format!("streaming blob PUT failed for '{key}' with HTTP {status}"),
                status,
            ))
        }
        .instrument(span.clone())
        .await;

        match result {
            Ok(status) => {
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation,
                    backend: "s3",
                    outcome: "ok",
                    status: Some(status),
                    request_bytes: content_len,
                    response_bytes: 0,
                });
                Ok(())
            }
            Err(error) => {
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation,
                    backend: "s3",
                    outcome: "error",
                    status: error.status,
                    request_bytes: content_len,
                    response_bytes: 0,
                });
                Err(error.message)
            }
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let started_at = Instant::now(); // determinism-ok: production blob transport metric only
        let span = blob_transport_span("get", "s3", 0);
        let result = async {
            let url = self.object_url(key);
            let mut request = self.client.get(&url);
            let headers = self
                .signed_headers(Method::GET, &url)
                .map_err(BlobTransportError::message)?;
            for (header_name, header_value) in &headers {
                request = request.header(header_name, header_value);
            }

            let response = request.send().await.map_err(|e| {
                BlobTransportError::message(format!("blob GET request failed for '{key}': {e}"))
            })?;
            let status = response.status();
            if status == StatusCode::NOT_FOUND {
                return Ok((status, None));
            }
            if !status.is_success() {
                return Err(BlobTransportError::status(
                    format!("blob GET failed for '{key}' with HTTP {status}"),
                    status,
                ));
            }

            let bytes = response.bytes().await.map_err(|e| {
                BlobTransportError::message(format!("blob GET body read failed for '{key}': {e}"))
            })?;
            Ok((status, Some(bytes.to_vec())))
        }
        .instrument(span.clone())
        .await;

        match result {
            Ok((status, bytes)) => {
                let response_bytes = bytes.as_ref().map_or(0, |bytes| bytes.len() as u64);
                let outcome = if bytes.is_some() { "ok" } else { "not_found" };
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation: "get",
                    backend: "s3",
                    outcome,
                    status: Some(status),
                    request_bytes: 0,
                    response_bytes,
                });
                Ok(bytes)
            }
            Err(error) => {
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation: "get",
                    backend: "s3",
                    outcome: "error",
                    status: error.status,
                    request_bytes: 0,
                    response_bytes: 0,
                });
                Err(error.message)
            }
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, String> {
        let started_at = Instant::now(); // determinism-ok: production blob transport metric only
        let span = blob_transport_span("head", "s3", 0);
        let result = async {
            let url = self.object_url(key);
            let mut request = self.client.head(&url);
            let headers = self
                .signed_headers(Method::HEAD, &url)
                .map_err(BlobTransportError::message)?;
            for (header_name, header_value) in &headers {
                request = request.header(header_name, header_value);
            }

            let response = request.send().await.map_err(|e| {
                BlobTransportError::message(format!("blob HEAD request failed for '{key}': {e}"))
            })?;
            let status = response.status();
            match status {
                StatusCode::OK | StatusCode::NO_CONTENT => Ok((status, true)),
                StatusCode::NOT_FOUND => Ok((status, false)),
                status if status.is_success() => Ok((status, true)),
                status => Err(BlobTransportError::status(
                    format!("blob HEAD failed for '{key}' with HTTP {status}"),
                    status,
                )),
            }
        }
        .instrument(span.clone())
        .await;

        match result {
            Ok((status, exists)) => {
                let outcome = if exists { "ok" } else { "not_found" };
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation: "head",
                    backend: "s3",
                    outcome,
                    status: Some(status),
                    request_bytes: 0,
                    response_bytes: 0,
                });
                Ok(exists)
            }
            Err(error) => {
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation: "head",
                    backend: "s3",
                    outcome: "error",
                    status: error.status,
                    request_bytes: 0,
                    response_bytes: 0,
                });
                Err(error.message)
            }
        }
    }

    fn object_url(&self, key: &str) -> String {
        let key = key.trim_start_matches('/');
        let object_path = self
            .key_prefix
            .as_deref()
            .map_or_else(|| key.to_string(), |prefix| format!("{prefix}/{key}"));
        format!("{}/{}/{}", self.endpoint, self.bucket, object_path)
    }

    fn signed_headers(&self, method: Method, url: &str) -> Result<HeaderMap, String> {
        let Some(access_key) = self.access_key.as_deref() else {
            return Ok(HeaderMap::new());
        };
        let Some(secret_key) = self.secret_key.as_deref() else {
            return Ok(HeaderMap::new());
        };

        let amz_date = aws_sigv4::amz_date_now();
        aws_sigv4::build_signed_headers(&aws_sigv4::SignedHeaderRequest {
            method: method.as_str(),
            url,
            payload_hash: "UNSIGNED-PAYLOAD",
            region: "auto",
            service: "s3",
            access_key,
            secret_key,
            amz_date: &amz_date,
            extra_signed_headers: &[],
            error_context: "blob",
        })
    }
}
