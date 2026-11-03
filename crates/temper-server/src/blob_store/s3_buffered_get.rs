//! Buffered S3 GET: one shared client, a short header and idle deadline, one
//! retry, and a bounded cache of immutable bodies.

use std::time::Instant;

use reqwest::{Method, StatusCode};
use tracing::Instrument as _;

use super::S3BlobStore;
use super::http_client::{self, BufferedGet};
use crate::blob_transport_observability::{
    BlobTransportFinish, blob_transport_span, finish_blob_transport,
};

impl S3BlobStore {
    pub(super) async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let cache_key = self.blob_cache_key(key);
        if let Some(bytes) = http_client::cached_blob(&cache_key) {
            self.record_get(Some(StatusCode::OK), "cache", bytes.len() as u64);
            return Ok(Some(bytes));
        }

        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self.get_once(key).await {
                Ok(None) => return Ok(None),
                Ok(Some(bytes)) => {
                    http_client::store_cached_blob(&cache_key, &bytes);
                    return Ok(Some(bytes));
                }
                Err(failure) if failure.retryable && attempt < http_client::BLOB_GET_ATTEMPTS => {}
                Err(failure) => return Err(failure.message),
            }
        }
    }

    async fn get_once(
        &self,
        key: &str,
    ) -> Result<Option<Vec<u8>>, http_client::BufferedGetFailure> {
        let url = self.object_url(key);
        let started_at = Instant::now(); // determinism-ok: production blob transport metric only
        let span = blob_transport_span("get", "s3", 0);
        let result = async {
            let headers = self
                .signed_headers(Method::GET, &url)
                .map_err(http_client::BufferedGetFailure::fatal)?;
            http_client::send_buffered_get(&self.client, &url, &headers, key).await
        }
        .instrument(span.clone())
        .await;

        match result {
            Ok(BufferedGet::Found { status, bytes }) => {
                let response_bytes = bytes.len() as u64;
                self.record_get_at(started_at, &span, Some(status), "ok", response_bytes);
                Ok(Some(bytes))
            }
            Ok(BufferedGet::Missing { status }) => {
                self.record_get_at(started_at, &span, Some(status), "not_found", 0);
                Ok(None)
            }
            Err(failure) => {
                self.record_get_at(started_at, &span, failure.status, "error", 0);
                Err(failure)
            }
        }
    }

    fn record_get(&self, status: Option<StatusCode>, outcome: &'static str, response_bytes: u64) {
        let started_at = Instant::now(); // determinism-ok: production blob transport metric only
        let span = blob_transport_span("get", "s3", 0);
        self.record_get_at(started_at, &span, status, outcome, response_bytes);
    }

    fn record_get_at(
        &self,
        started_at: Instant,
        span: &tracing::Span,
        status: Option<StatusCode>,
        outcome: &str,
        response_bytes: u64,
    ) {
        finish_blob_transport(BlobTransportFinish {
            started_at,
            span,
            operation: "get",
            backend: "s3",
            outcome,
            status,
            request_bytes: 0,
            response_bytes,
        });
    }

    fn blob_cache_key(&self, key: &str) -> String {
        let prefix = self.key_prefix.as_deref().unwrap_or("");
        format!("{}\n{}\n{}\n{}", self.endpoint, self.bucket, prefix, key)
    }
}
