use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use futures_util::StreamExt as _;
use reqwest::{Method, StatusCode};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;
use tokio::sync::OwnedSemaphorePermit;
use tracing::Instrument as _;

use super::{
    BLOB_IO_QUEUE_TIMEOUT, BlobByteStream, BlobStore, BlobStoreBackend, S3BlobStore,
    blob_io_semaphore, keys::hex_lower, local_blob_path,
};
use crate::blob_store::local::get_local_blob_bounded_observed;
use crate::blob_transport_observability::{
    BlobTransportError, BlobTransportFinish, blob_transport_span, finish_blob_transport,
};

const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const BASE64_INPUT_CHUNK_BYTES: usize = 64 * 1024;
const BLOB_STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const BLOB_BOUNDED_READ_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const BLOB_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const BLOB_STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_BLOB_STREAM_MAX_CONCURRENCY: usize = 32;

fn blob_stream_semaphore() -> Arc<tokio::sync::Semaphore> {
    static SEMAPHORE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    Arc::clone(SEMAPHORE.get_or_init(|| {
        let limit = std::env::var("TEMPER_BLOB_STREAM_MAX_CONCURRENCY") // determinism-ok: startup-only tuning knob
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_BLOB_STREAM_MAX_CONCURRENCY);
        Arc::new(tokio::sync::Semaphore::new(limit))
    }))
}

#[derive(Debug)]
pub(crate) enum BlobReadBounded {
    Found(Vec<u8>),
    Missing,
    TooLarge { actual_bytes: Option<u64> },
}

/// Streaming object-store response with a declared byte length.
pub struct BlobObjectStream {
    content_length: u64,
    stream: BlobByteStream,
}

impl BlobObjectStream {
    /// Number of bytes the stream must yield before completing.
    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    /// Consume this descriptor and return its bounded byte stream.
    pub fn into_stream(self) -> BlobByteStream {
        self.stream
    }

    /// Verify the serialized object's content-addressed SHA-256 while reading.
    pub fn verify_sha256(self, expected_sha256: &str) -> Self {
        let content_length = self.content_length;
        let expected_sha256 = expected_sha256.to_string();
        let mut source = self.stream;
        let stream = Box::pin(async_stream::try_stream! {
            let mut hasher = Sha256::new();
            while let Some(chunk) = source.next().await {
                let chunk = chunk?;
                hasher.update(&chunk);
                yield chunk;
            }
            let actual = hex_lower(&hasher.finalize());
            if actual != expected_sha256 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "field-overflow object failed SHA-256 verification",
                ))?;
            }
        });
        Self {
            content_length,
            stream,
        }
    }

    fn hold_stream_permit(self, permit: OwnedSemaphorePermit) -> Self {
        let content_length = self.content_length;
        let mut source = self.stream;
        let stream = Box::pin(async_stream::try_stream! {
            let _permit = permit;
            while let Some(chunk) = source.next().await {
                yield chunk?;
            }
        });
        Self {
            content_length,
            stream,
        }
    }
}

/// Result of opening an object as a bounded stream.
pub enum BlobStreamRead {
    /// Object exists and is within the caller's encoded-size boundary.
    Found(BlobObjectStream),
    /// Object does not exist.
    Missing,
    /// Object metadata exceeds the caller's boundary.
    TooLarge { actual_bytes: Option<u64> },
}

impl BlobStore {
    pub(crate) async fn get_bounded(
        &self,
        key: &str,
        max_bytes: usize,
    ) -> Result<BlobReadBounded, String> {
        let queued_at = Instant::now(); // determinism-ok: production blob I/O queue metric only
        let _permit =
            tokio::time::timeout(BLOB_IO_QUEUE_TIMEOUT, blob_io_semaphore().acquire_owned())
                .await
                .map_err(|_| "bounded blob read queue deadline exceeded".to_string())?
                .expect("blob semaphore closed"); // ci-ok: process-global and never closed
        crate::runtime_metrics::record_blob_io_wait_duration(queued_at.elapsed(), "get_bounded");

        match &self.backend {
            BlobStoreBackend::LocalFs { root } => tokio::time::timeout(
                BLOB_BOUNDED_READ_TIMEOUT,
                get_local_blob_bounded_observed(root, key, max_bytes),
            )
            .await
            .map_err(|_| format!("bounded local blob read timed out for '{key}'"))?,
            BlobStoreBackend::S3(store) => store.get_bounded(key, max_bytes).await,
        }
    }

    /// Open an object as a bounded stream without buffering it in memory.
    pub(crate) async fn get_stream(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<BlobStreamRead, String> {
        let queued_at = Instant::now(); // determinism-ok: production blob I/O queue metric only
        let stream_permit = tokio::time::timeout(
            BLOB_IO_QUEUE_TIMEOUT,
            blob_stream_semaphore().acquire_owned(),
        )
        .await
        .map_err(|_| "blob stream concurrency queue deadline exceeded".to_string())?
        .expect("blob semaphore closed"); // ci-ok: process-global and never closed
        let _io_permit =
            tokio::time::timeout(BLOB_IO_QUEUE_TIMEOUT, blob_io_semaphore().acquire_owned())
                .await
                .map_err(|_| "blob I/O queue deadline exceeded".to_string())?
                .expect("blob semaphore closed"); // ci-ok: process-global and never closed
        crate::runtime_metrics::record_blob_io_wait_duration(queued_at.elapsed(), "get_stream");
        let opened = match &self.backend {
            BlobStoreBackend::LocalFs { root } => open_local_stream(root, key, max_bytes).await,
            BlobStoreBackend::S3(store) => store.get_stream(key, max_bytes).await,
        }?;
        Ok(match opened {
            BlobStreamRead::Found(stream) => {
                BlobStreamRead::Found(stream.hold_stream_permit(stream_permit))
            }
            BlobStreamRead::Missing => BlobStreamRead::Missing,
            BlobStreamRead::TooLarge { actual_bytes } => BlobStreamRead::TooLarge { actual_bytes },
        })
    }
}

impl S3BlobStore {
    async fn get_bounded(&self, key: &str, max_bytes: usize) -> Result<BlobReadBounded, String> {
        let started_at = Instant::now(); // determinism-ok: production blob transport metric only
        let span = blob_transport_span("get_bounded", "s3", 0);
        let result = async {
            let url = self.object_url(key);
            let mut request = self.client.get(&url).timeout(BLOB_BOUNDED_READ_TIMEOUT);
            let headers = self
                .signed_headers(Method::GET, &url)
                .map_err(BlobTransportError::message)?;
            for (header_name, header_value) in &headers {
                request = request.header(header_name, header_value);
            }

            let response = request.send().await.map_err(|error| {
                BlobTransportError::message(format!(
                    "bounded blob GET request failed for '{key}': {error}"
                ))
            })?;
            let status = response.status();
            if status == StatusCode::NOT_FOUND {
                return Ok((status, BlobReadBounded::Missing));
            }
            if !status.is_success() {
                return Err(BlobTransportError::status(
                    format!("bounded blob GET failed for '{key}' with HTTP {status}"),
                    status,
                ));
            }
            if let Some(actual_bytes) = response.content_length()
                && actual_bytes > max_bytes as u64
            {
                return Ok((
                    status,
                    BlobReadBounded::TooLarge {
                        actual_bytes: Some(actual_bytes),
                    },
                ));
            }

            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| {
                    BlobTransportError::message(format!(
                        "bounded blob GET body failed for '{key}': {error}"
                    ))
                })?;
                if bytes.len().saturating_add(chunk.len()) > max_bytes {
                    return Ok((status, BlobReadBounded::TooLarge { actual_bytes: None }));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok((status, BlobReadBounded::Found(bytes)))
        }
        .instrument(span.clone())
        .await;

        match result {
            Ok((status, outcome)) => {
                let (label, response_bytes) = match &outcome {
                    BlobReadBounded::Found(bytes) => ("ok", bytes.len() as u64),
                    BlobReadBounded::Missing => ("not_found", 0),
                    BlobReadBounded::TooLarge { .. } => ("too_large", 0),
                };
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation: "get_bounded",
                    backend: "s3",
                    outcome: label,
                    status: Some(status),
                    request_bytes: 0,
                    response_bytes,
                });
                Ok(outcome)
            }
            Err(error) => {
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation: "get_bounded",
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

    async fn get_stream(&self, key: &str, max_bytes: u64) -> Result<BlobStreamRead, String> {
        let url = self.object_url(key);
        let mut request = self.client.get(&url).timeout(BLOB_STREAM_TOTAL_TIMEOUT);
        let headers = self.signed_headers(Method::GET, &url)?;
        for (header_name, header_value) in &headers {
            request = request.header(header_name, header_value);
        }
        let response = tokio::time::timeout(BLOB_STREAM_OPEN_TIMEOUT, request.send())
            .await
            .map_err(|_| format!("streaming blob GET open timed out for '{key}'"))?
            .map_err(|error| format!("streaming blob GET request failed for '{key}': {error}"))?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(BlobStreamRead::Missing);
        }
        if !status.is_success() {
            return Err(format!(
                "streaming blob GET failed for '{key}' with HTTP {status}"
            ));
        }
        let Some(content_length) = response.content_length() else {
            return Err(format!(
                "streaming blob GET for '{key}' omitted Content-Length"
            ));
        };
        if content_length > max_bytes {
            return Ok(BlobStreamRead::TooLarge {
                actual_bytes: Some(content_length),
            });
        }
        let source: BlobByteStream = Box::pin(
            response
                .bytes_stream()
                .map(|item| item.map_err(std::io::Error::other)),
        );
        Ok(BlobStreamRead::Found(BlobObjectStream {
            content_length,
            stream: enforce_stream_bounds(source, content_length),
        }))
    }
}

async fn open_local_stream(
    root: &std::path::Path,
    key: &str,
    max_bytes: u64,
) -> Result<BlobStreamRead, String> {
    let path = local_blob_path(root, key)?;
    let file = match tokio::time::timeout(BLOB_STREAM_OPEN_TIMEOUT, tokio::fs::File::open(&path))
        .await
        .map_err(|_| format!("opening local blob '{}' timed out", path.display()))?
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BlobStreamRead::Missing);
        }
        Err(error) => {
            return Err(format!(
                "failed to open local blob '{}': {error}",
                path.display()
            ));
        }
    };
    let content_length = tokio::time::timeout(BLOB_STREAM_OPEN_TIMEOUT, file.metadata())
        .await
        .map_err(|_| format!("stating local blob '{}' timed out", path.display()))?
        .map_err(|error| format!("failed to stat local blob '{}': {error}", path.display()))?
        .len();
    if content_length > max_bytes {
        return Ok(BlobStreamRead::TooLarge {
            actual_bytes: Some(content_length),
        });
    }
    let source: BlobByteStream = Box::pin(async_stream::try_stream! {
        let mut file = file;
        let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            yield Bytes::copy_from_slice(&buffer[..read]);
        }
    });
    Ok(BlobStreamRead::Found(BlobObjectStream {
        content_length,
        stream: enforce_stream_bounds(source, content_length),
    }))
}

fn enforce_stream_bounds(mut source: BlobByteStream, expected_bytes: u64) -> BlobByteStream {
    Box::pin(async_stream::try_stream! {
        let started = tokio::time::Instant::now(); // determinism-ok: production object-store I/O deadline
        let deadline = started + BLOB_STREAM_TOTAL_TIMEOUT;
        let mut emitted = 0u64;
        loop {
            let now = tokio::time::Instant::now(); // determinism-ok: production object-store I/O deadline
            if now >= deadline {
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "blob stream total deadline exceeded"))?;
            }
            let wait = BLOB_STREAM_IDLE_TIMEOUT.min(deadline.saturating_duration_since(now));
            let next = tokio::time::timeout(wait, source.next())
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "blob stream idle deadline exceeded"))?;
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk?;
            emitted = emitted
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| std::io::Error::other("blob stream byte count overflow"))?;
            if emitted > expected_bytes {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "blob stream exceeded Content-Length"))?;
            }
            if !chunk.is_empty() {
                yield chunk;
            }
        }
        if emitted != expected_bytes {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("blob stream ended at {emitted} bytes; expected {expected_bytes}"),
            ))?;
        }
    })
}

/// Incrementally decode a JSON string containing standard base64.
pub fn decode_json_base64_stream(
    encoded: BlobObjectStream,
    expected_decoded_bytes: u64,
) -> BlobObjectStream {
    let mut source = encoded.into_stream();
    let stream: BlobByteStream = Box::pin(async_stream::try_stream! {
        let mut opened = false;
        let mut pending = None;
        let mut encoded_buffer = Vec::with_capacity(BASE64_INPUT_CHUNK_BYTES);
        let mut decoded_bytes = 0u64;
        let mut padding_seen = false;

        while let Some(chunk) = source.next().await {
            let chunk = chunk?;
            for byte in chunk {
                if !opened {
                    if byte != b'"' {
                        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "overflow blob is not a JSON string"))?;
                    }
                    opened = true;
                    continue;
                }
                if let Some(previous) = pending.replace(byte) {
                    push_base64_byte(previous, &mut encoded_buffer, padding_seen)?;
                }
                if encoded_buffer.len() == BASE64_INPUT_CHUNK_BYTES {
                    let group = base64::engine::general_purpose::STANDARD
                        .decode(&encoded_buffer)
                        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                    decoded_bytes = decoded_bytes
                        .checked_add(group.len() as u64)
                        .ok_or_else(|| std::io::Error::other("decoded blob byte count overflow"))?;
                    if decoded_bytes > expected_decoded_bytes {
                        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "decoded blob exceeded expected length"))?;
                    }
                    padding_seen = encoded_buffer.contains(&b'=');
                    encoded_buffer.clear();
                    yield Bytes::from(group);
                }
            }
        }
        if !opened || pending != Some(b'"') {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "overflow blob JSON string is truncated"))?;
        }
        if !encoded_buffer.is_empty() {
            if encoded_buffer.len() % 4 != 0 {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "overflow blob has incomplete base64"))?;
            }
            let group = base64::engine::general_purpose::STANDARD
                .decode(&encoded_buffer)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            decoded_bytes = decoded_bytes
                .checked_add(group.len() as u64)
                .ok_or_else(|| std::io::Error::other("decoded blob byte count overflow"))?;
            if decoded_bytes > expected_decoded_bytes {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "decoded blob exceeded expected length"))?;
            }
            yield Bytes::from(group);
        }
        if decoded_bytes != expected_decoded_bytes {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("decoded blob ended at {decoded_bytes} bytes; expected {expected_decoded_bytes}"),
            ))?;
        }
    });
    BlobObjectStream {
        content_length: expected_decoded_bytes,
        stream,
    }
}

fn push_base64_byte(
    byte: u8,
    encoded_buffer: &mut Vec<u8>,
    padding_seen: bool,
) -> Result<(), std::io::Error> {
    if padding_seen {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "base64 data followed padding",
        ));
    }
    if !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "overflow blob contains non-base64 data",
        ));
    }
    encoded_buffer.push(byte);
    Ok(())
}

#[cfg(test)]
#[path = "streaming/tests.rs"]
mod tests;
