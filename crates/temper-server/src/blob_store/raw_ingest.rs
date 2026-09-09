use std::io::Write as _;
use std::pin::Pin;
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use sha1::Digest as _;
use sha2::Sha256;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracing::Instrument as _;

use super::{
    BLOB_IO_QUEUE_TIMEOUT, BlobStore, BlobStoreBackend, blob_io_semaphore, keys::hex_lower,
    local_blob_path,
};
use crate::blob_transport_observability::{
    BlobTransportFinish, blob_transport_span, finish_blob_transport,
};

const STREAM_CHUNK_BYTES: usize = 64 * 1024;
pub(super) const BLOB_BACKEND_OPERATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(crate) const MAX_RAW_BLOB_BYTES: usize = 2 * 1024 * 1024 * 1024;

mod admission;
pub(crate) use admission::{
    BlobIngestAdmissionError, BlobIngestBudget, BlobIngestPermit, BlobIngestProgressPolicy,
    BlobStageError,
};

/// Bounded asynchronous byte stream used at the object-store boundary.
pub type BlobByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static>>;

/// Disk-backed body whose temporary file is deleted when this value drops.
pub(crate) struct StagedBlob {
    path: tempfile::TempPath,
    declared_len: usize,
    canonical_sha1: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Base64JsonDescriptor {
    pub(crate) sha256: String,
    pub(crate) serialized_len: usize,
}

impl BlobStore {
    /// Copy an untrusted body stream into an RAII staging file while hashing
    /// the caller-supplied canonical prefix and exact declared body bytes.
    pub(crate) async fn stage_canonical_stream(
        &self,
        mut stream: BlobByteStream,
        declared_len: usize,
        canonical_prefix: &[u8],
        progress: &BlobIngestProgressPolicy,
        admission: &mut BlobIngestPermit,
    ) -> Result<StagedBlob, BlobStageError> {
        let started = tokio::time::Instant::now(); // determinism-ok: production upload deadline
        let total_deadline = started + progress.total_timeout;
        tokio::time::timeout_at(
            total_deadline,
            tokio::fs::create_dir_all(&self.staging_root),
        )
        .await
        .map_err(|_| BlobStageError::TotalDeadline { received: 0 })?
        .map_err(|error| {
            BlobStageError::Storage(format!(
                "failed to create blob staging directory '{}': {error}",
                self.staging_root.display()
            ))
        })?;
        let staging_root = self.staging_root.clone();
        let staged = tokio::time::timeout_at(
            total_deadline,
            // determinism-ok: production object-store filesystem boundary
            tokio::task::spawn_blocking(move || {
                tempfile::Builder::new()
                    .prefix("raw-ingest-")
                    .tempfile_in(staging_root)
            }),
        )
        .await
        .map_err(|_| BlobStageError::TotalDeadline { received: 0 })?
        .map_err(|error| BlobStageError::Storage(format!("blob staging task failed: {error}")))?
        .map_err(|error| {
            BlobStageError::Storage(format!(
                "failed to create blob staging file in '{}': {error}",
                self.staging_root.display()
            ))
        })?;
        let (file, path) = staged.into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let mut canonical_hasher = sha1::Sha1::new();
        canonical_hasher.update(canonical_prefix);
        let mut received = 0usize;
        let body_started = tokio::time::Instant::now(); // determinism-ok: production upload progress
        let mut last_progress = body_started;
        let mut throughput_check = body_started + progress.throughput_grace;

        loop {
            let next = tokio::select! {
                _ = tokio::time::sleep_until(total_deadline) => {
                    return Err(BlobStageError::TotalDeadline { received });
                }
                _ = tokio::time::sleep_until(last_progress + progress.idle_timeout) => {
                    return Err(BlobStageError::IdleTimeout { received });
                }
                _ = tokio::time::sleep_until(throughput_check) => {
                    let elapsed = tokio::time::Instant::now() // determinism-ok: production upload throughput clock
                        .saturating_duration_since(body_started);
                    let elapsed_millis = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
                    let required = progress
                        .min_bytes_per_second
                        .saturating_mul(elapsed_millis)
                        / 1000;
                    if (received as u64) < required {
                        return Err(BlobStageError::ThroughputTooLow { received, required });
                    }
                    throughput_check += progress.throughput_check_interval;
                    continue;
                }
                next = stream.next() => next,
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk.map_err(|error| BlobStageError::BodyStream(error.to_string()))?;
            if chunk.is_empty() {
                tokio::task::yield_now().await;
                continue;
            }
            received = received.checked_add(chunk.len()).ok_or_else(|| {
                BlobStageError::Storage("body byte count overflowed usize".to_string())
            })?;
            if received > declared_len {
                return Err(BlobStageError::BodyExceedsDeclaredLength {
                    declared: declared_len,
                });
            }
            admission.reserve_received_bytes(received)?;
            tokio::time::timeout_at(total_deadline, file.write_all(&chunk))
                .await
                .map_err(|_| BlobStageError::TotalDeadline { received })?
                .map_err(|error| {
                    BlobStageError::Storage(format!("failed to write staged Blob body: {error}"))
                })?;
            canonical_hasher.update(&chunk);
            last_progress = tokio::time::Instant::now(); // determinism-ok: production upload progress
        }
        if received != declared_len {
            return Err(BlobStageError::BodyShorterThanDeclaredLength {
                declared: declared_len,
                received,
            });
        }
        tokio::time::timeout_at(total_deadline, file.flush())
            .await
            .map_err(|_| BlobStageError::TotalDeadline { received })?
            .map_err(|error| {
                BlobStageError::Storage(format!("failed to flush staged Blob body: {error}"))
            })?;
        tokio::time::timeout_at(total_deadline, file.sync_data())
            .await
            .map_err(|_| BlobStageError::TotalDeadline { received })?
            .map_err(|error| {
                BlobStageError::Storage(format!("failed to sync staged Blob body: {error}"))
            })?;
        Ok(StagedBlob {
            path,
            declared_len,
            canonical_sha1: hex_lower(&canonical_hasher.finalize()),
        })
    }

    /// Stream JSON base64(prefix || staged bytes) into the object store.
    pub(crate) async fn put_staged_base64_json(
        &self,
        key: &str,
        staged: &StagedBlob,
        prefix: &[u8],
        serialized_len: usize,
    ) -> Result<(), String> {
        let expected_len = staged.base64_json_len(prefix)?;
        if serialized_len != expected_len {
            return Err(format!(
                "base64 JSON length mismatch: descriptor {serialized_len}, expected {expected_len}"
            ));
        }
        let stream = staged.base64_json_stream(prefix.to_vec());
        self.put_content_addressed_stream(key, stream, serialized_len as u64)
            .await
    }

    async fn put_content_addressed_stream(
        &self,
        key: &str,
        stream: BlobByteStream,
        content_len: u64,
    ) -> Result<(), String> {
        let queued_at = Instant::now(); // determinism-ok: production blob I/O queue metric only
        let _permit =
            tokio::time::timeout(BLOB_IO_QUEUE_TIMEOUT, blob_io_semaphore().acquire_owned())
                .await
                .map_err(|_| "blob object-store queue deadline exceeded".to_string())?
                .expect("blob semaphore closed"); // ci-ok: process-global and never closed
        crate::runtime_metrics::record_blob_io_wait_duration(
            queued_at.elapsed(),
            "put_content_stream",
        );
        let stream = enforce_outgoing_length(stream, content_len);
        let operation = async {
            match &self.backend {
                BlobStoreBackend::LocalFs { root } => {
                    put_local_blob_stream_observed(root, key, stream, content_len).await
                }
                BlobStoreBackend::S3(store) => {
                    store
                        .put_stream_with_operation("put_content_stream", key, stream, content_len)
                        .await
                }
            }
        };
        tokio::time::timeout(BLOB_BACKEND_OPERATION_TIMEOUT, operation)
            .await
            .map_err(|_| "blob object-store write deadline exceeded".to_string())?
    }
}

fn enforce_outgoing_length(mut stream: BlobByteStream, expected_bytes: u64) -> BlobByteStream {
    Box::pin(async_stream::try_stream! {
        let mut emitted = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            emitted = emitted
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| std::io::Error::other("outgoing blob byte count overflow"))?;
            if emitted > expected_bytes {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "outgoing blob stream exceeded its declared length",
                ))?;
            }
            if !chunk.is_empty() {
                yield chunk;
            }
        }
        if emitted != expected_bytes {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("outgoing blob stream ended at {emitted} bytes; expected {expected_bytes}"),
            ))?;
        }
    })
}

impl StagedBlob {
    pub(crate) fn canonical_sha1(&self) -> &str {
        &self.canonical_sha1
    }

    pub(crate) async fn base64_json_descriptor(
        &self,
        prefix: &[u8],
    ) -> Result<Base64JsonDescriptor, String> {
        tokio::time::timeout(
            BLOB_BACKEND_OPERATION_TIMEOUT,
            self.base64_json_descriptor_inner(prefix),
        )
        .await
        .map_err(|_| "blob descriptor computation deadline exceeded".to_string())?
    }

    async fn base64_json_descriptor_inner(
        &self,
        prefix: &[u8],
    ) -> Result<Base64JsonDescriptor, String> {
        let serialized_len = self.base64_json_len(prefix)?;
        let mut file = tokio::fs::File::open(self.path.to_path_buf())
            .await
            .map_err(|error| format!("failed to reopen staged Blob body: {error}"))?;
        let mut hasher = Sha256::new();
        hasher.update(b"\"");
        {
            let sink = DigestWriter(&mut hasher);
            let mut encoder =
                base64::write::EncoderWriter::new(sink, &base64::engine::general_purpose::STANDARD);
            encoder
                .write_all(prefix)
                .map_err(|error| format!("failed to hash canonical base64 prefix: {error}"))?;
            let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
            loop {
                let read = file
                    .read(&mut buffer)
                    .await
                    .map_err(|error| format!("failed to read staged Blob body: {error}"))?;
                if read == 0 {
                    break;
                }
                encoder
                    .write_all(&buffer[..read])
                    .map_err(|error| format!("failed to hash staged base64 bytes: {error}"))?;
            }
            encoder
                .finish()
                .map_err(|error| format!("failed to finish staged base64 hash: {error}"))?;
        }
        hasher.update(b"\"");
        Ok(Base64JsonDescriptor {
            sha256: hex_lower(&hasher.finalize()),
            serialized_len,
        })
    }

    fn base64_json_len(&self, prefix: &[u8]) -> Result<usize, String> {
        let raw_len = prefix
            .len()
            .checked_add(self.declared_len)
            .ok_or_else(|| "canonical Blob representation length overflowed usize".to_string())?;
        base64::encoded_len(raw_len, true)
            .and_then(|encoded| encoded.checked_add(2))
            .ok_or_else(|| "base64 JSON representation length overflowed usize".to_string())
    }

    fn base64_json_stream(&self, prefix: Vec<u8>) -> BlobByteStream {
        let path = self.path.to_path_buf();
        Box::pin(async_stream::try_stream! {
            yield Bytes::from_static(b"\"");
            let mut encoder = Base64ChunkEncoder::default();
            if let Some(encoded) = encoder.push(&prefix) {
                yield encoded;
            }
            let mut file = tokio::fs::File::open(&path).await?;
            let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
            loop {
                let read = file.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                if let Some(encoded) = encoder.push(&buffer[..read]) {
                    yield encoded;
                }
            }
            if let Some(encoded) = encoder.finish() {
                yield encoded;
            }
            yield Bytes::from_static(b"\"");
        })
    }
}

struct DigestWriter<'a>(&'a mut Sha256);

impl std::io::Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Base64ChunkEncoder {
    carry: Vec<u8>,
}

impl Base64ChunkEncoder {
    fn push(&mut self, input: &[u8]) -> Option<Bytes> {
        if input.is_empty() {
            return None;
        }
        let mut combined = Vec::with_capacity(self.carry.len() + input.len());
        combined.extend_from_slice(&self.carry);
        combined.extend_from_slice(input);
        let complete_len = (combined.len() / 3) * 3;
        self.carry.clear();
        self.carry.extend_from_slice(&combined[complete_len..]);
        if complete_len == 0 {
            return None;
        }
        Some(Bytes::from(
            base64::engine::general_purpose::STANDARD.encode(&combined[..complete_len]),
        ))
    }

    fn finish(&mut self) -> Option<Bytes> {
        if self.carry.is_empty() {
            return None;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&self.carry);
        self.carry.clear();
        Some(Bytes::from(encoded))
    }
}

async fn put_local_blob_stream(
    root: &std::path::Path,
    key: &str,
    mut stream: BlobByteStream,
    expected_bytes: u64,
) -> Result<(), String> {
    let path = local_blob_path(root, key)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("local blob '{}' has no parent directory", path.display()))?;
    tokio::fs::create_dir_all(parent).await.map_err(|error| {
        format!(
            "failed to create local blob dir '{}': {error}",
            parent.display()
        )
    })?;
    let parent = parent.to_path_buf();
    let staged = tokio::time::timeout(
        BLOB_IO_QUEUE_TIMEOUT,
        // determinism-ok: production object-store filesystem boundary
        tokio::task::spawn_blocking(move || {
            tempfile::Builder::new()
                .prefix("object-put-")
                .tempfile_in(parent)
        }),
    )
    .await
    .map_err(|_| "local blob staging-file creation timed out".to_string())?
    .map_err(|error| format!("local blob staging task failed: {error}"))?
    .map_err(|error| format!("failed to create local blob staging file: {error}"))?;
    let (file, staged_path) = staged.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        written = written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| "local blob byte count overflowed u64".to_string())?;
        if written > expected_bytes {
            return Err(format!(
                "local blob stream exceeded declared length {expected_bytes}"
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("failed to write local blob '{}': {error}", path.display()))?;
    }
    if written != expected_bytes {
        return Err(format!(
            "local blob stream ended at {written} bytes; expected {expected_bytes}"
        ));
    }
    file.flush().await.map_err(|error| error.to_string())?;
    file.sync_data().await.map_err(|error| error.to_string())?;
    drop(file);
    tokio::fs::rename(staged_path.to_path_buf(), &path)
        .await
        .map_err(|error| format!("failed to publish local blob '{}': {error}", path.display()))
}

async fn put_local_blob_stream_observed(
    root: &std::path::Path,
    key: &str,
    stream: BlobByteStream,
    request_bytes: u64,
) -> Result<(), String> {
    let started_at = Instant::now(); // determinism-ok: production blob transport metric only
    let span = blob_transport_span("put_content_stream", "local_fs", request_bytes);
    let result = put_local_blob_stream(root, key, stream, request_bytes)
        .instrument(span.clone())
        .await;
    finish_blob_transport(BlobTransportFinish {
        started_at,
        span: &span,
        operation: "put_content_stream",
        backend: "local_fs",
        outcome: if result.is_ok() { "ok" } else { "error" },
        status: None,
        request_bytes,
        response_bytes: 0,
    });
    result
}

#[cfg(test)]
#[path = "raw_ingest/tests.rs"]
mod tests;
