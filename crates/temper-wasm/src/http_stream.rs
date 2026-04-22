//! Bidirectional HTTP streaming primitives for WASM integrations.
//!
//! Implements ADR-0057's `http_call_streaming` surface. The kernel
//! owns a `HttpStreamRegistry` of bounded `tokio::sync::mpsc` channels;
//! each handle ID maps to one channel. WASM guests open a streaming
//! exchange via `http_stream_begin_outbound`, then push request body
//! chunks through the request handle and pull response body chunks
//! through the response handle. Head metadata (status, headers) is
//! delivered as a separate `HttpResponseHead` once the request is
//! sent.
//!
//! Bounded channels give us backpressure: when a handle's channel is
//! full, writes return [`StreamError::WouldBlock`] and the guest is
//! expected to yield cooperatively. When a channel is closed (by
//! explicit close, request completion, or timeout), reads/writes
//! return [`StreamError::Closed`].
//!
//! Capacity: 64 chunks × 16 KiB = 1 MiB per handle. A full exchange
//! (request + response) thus caps at 2 MiB of in-flight bytes — well
//! within ADR-0057's <4 MiB per-request budget.

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Chunk size used by SDK adapters when splitting writes. A single
/// chunk may be smaller (short writes), but never larger — the
/// receiving side relies on this for its bookkeeping.
pub const STREAM_CHUNK_BYTES: usize = 16 * 1024;

/// Per-handle channel capacity in chunks. Total resident bytes per
/// handle = capacity × STREAM_CHUNK_BYTES. At 64 × 16 KiB this is
/// 1 MiB per handle, 2 MiB per bidirectional exchange.
pub const STREAM_CHANNEL_CAPACITY: usize = 64;

/// Opaque handle identifying one end of a streaming channel. Passed
/// from guest to host via FFI; host-side lookups go through
/// [`HttpStreamRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamHandle(pub u32);

/// Head metadata for an outbound streaming request. Body is streamed
/// separately through a [`StreamHandle`].
#[derive(Debug, Clone)]
pub struct HttpRequestHead {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Head metadata for an inbound streaming response (outbound call)
/// or outbound streaming response (inbound handler).
#[derive(Debug, Clone, Default)]
pub struct HttpResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// Pair of handles returned to a guest when it opens a streaming
/// exchange. The request_body handle is write-only (guest pushes
/// request chunks into it); the response_body handle is read-only
/// (guest pulls response chunks from it). See ADR-0057 Sub-Decision 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpStreamHandles {
    /// Guest writes request body chunks here. Host reads them.
    /// Close to signal end-of-request-body.
    pub request_body: StreamHandle,
    /// Guest reads response body chunks here. Empty chunk = EOF.
    pub response_body: StreamHandle,
}

/// Errors surfaced by stream read/write operations. Guests map these
/// to FFI return codes (negative ints) and `std::io::ErrorKind` in
/// the SDK adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// Channel is full (for writes) or empty (for reads with
    /// non-blocking semantics). Guest should yield and retry.
    WouldBlock,
    /// The other side of the channel hung up cleanly; remaining
    /// buffered bytes have already been drained.
    Closed,
    /// Timeout, abort, or peer fault. Not recoverable.
    Aborted(String),
    /// Handle is unknown to the registry (already freed or never
    /// existed). Guest bug.
    InvalidHandle,
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::WouldBlock => write!(f, "stream would block"),
            StreamError::Closed => write!(f, "stream closed"),
            StreamError::Aborted(msg) => write!(f, "stream aborted: {msg}"),
            StreamError::InvalidHandle => write!(f, "invalid stream handle"),
        }
    }
}

impl std::error::Error for StreamError {}

/// One end of a bounded stream channel. Sending side has the Sender;
/// receiving side has the Receiver (wrapped in Arc<Mutex<>> so
/// recv() can serialize across concurrent reads if needed).
enum ChannelEnd {
    Sender(mpsc::Sender<Vec<u8>>),
    Receiver(Arc<Mutex<mpsc::Receiver<Vec<u8>>>>),
}

/// Registry of active stream handles. Lives on `ProductionWasmHost`
/// (and any other host that implements streaming). One registry per
/// host instance; handle IDs are unique within a host but not across
/// hosts — they're opaque u32s.
pub struct HttpStreamRegistry {
    inner: Mutex<RegistryState>,
}

struct RegistryState {
    next_id: u32,
    handles: BTreeMap<u32, ChannelEnd>,
}

impl HttpStreamRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryState {
                next_id: 1,
                handles: BTreeMap::new(),
            }),
        }
    }

    /// Allocate a new paired (sender, receiver) channel. Returns
    /// (write_handle, read_handle) — guests write into the first,
    /// the other end reads from the second.
    pub async fn create_pair(&self) -> (StreamHandle, StreamHandle) {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(STREAM_CHANNEL_CAPACITY);
        let mut state = self.inner.lock().await;
        let write_id = state.next_id;
        state.next_id += 1;
        let read_id = state.next_id;
        state.next_id += 1;
        state.handles.insert(write_id, ChannelEnd::Sender(tx));
        state.handles.insert(
            read_id,
            ChannelEnd::Receiver(Arc::new(Mutex::new(rx))),
        );
        (StreamHandle(write_id), StreamHandle(read_id))
    }

    /// Write `chunk` to the handle's channel. Suspends if the
    /// channel is full (cooperative backpressure); use `try_write`
    /// for WouldBlock semantics.
    pub async fn write(&self, handle: StreamHandle, chunk: Vec<u8>) -> Result<usize, StreamError> {
        let bytes = chunk.len();
        let sender = self.sender_for(handle).await?;
        sender.send(chunk).await.map_err(|_| StreamError::Closed)?;
        Ok(bytes)
    }

    /// Non-blocking write. Returns WouldBlock if the channel is full.
    pub async fn try_write(
        &self,
        handle: StreamHandle,
        chunk: Vec<u8>,
    ) -> Result<usize, StreamError> {
        let bytes = chunk.len();
        let sender = self.sender_for(handle).await?;
        match sender.try_send(chunk) {
            Ok(()) => Ok(bytes),
            Err(mpsc::error::TrySendError::Full(_)) => Err(StreamError::WouldBlock),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(StreamError::Closed),
        }
    }

    /// Read the next chunk from the handle's receiver. Returns
    /// `Ok(Vec::new())` for clean EOF (peer closed after sending
    /// all data).
    pub async fn read(&self, handle: StreamHandle) -> Result<Vec<u8>, StreamError> {
        let rx_mutex = self.receiver_for(handle).await?;
        let mut rx = rx_mutex.lock().await;
        match rx.recv().await {
            Some(chunk) => Ok(chunk),
            None => Ok(Vec::new()), // clean EOF
        }
    }

    /// Close a handle, freeing its registry slot. The other side of
    /// the channel observes this as EOF on its next read or a
    /// Closed error on its next write.
    pub async fn close(&self, handle: StreamHandle) -> Result<(), StreamError> {
        let mut state = self.inner.lock().await;
        match state.handles.remove(&handle.0) {
            Some(_) => Ok(()),
            None => Err(StreamError::InvalidHandle),
        }
    }

    /// Current handle count — for metrics and leak detection in tests.
    pub async fn handle_count(&self) -> usize {
        self.inner.lock().await.handles.len()
    }

    // --- Private helpers ---

    async fn sender_for(
        &self,
        handle: StreamHandle,
    ) -> Result<mpsc::Sender<Vec<u8>>, StreamError> {
        let state = self.inner.lock().await;
        match state.handles.get(&handle.0) {
            Some(ChannelEnd::Sender(tx)) => Ok(tx.clone()),
            Some(ChannelEnd::Receiver(_)) => Err(StreamError::InvalidHandle),
            None => Err(StreamError::InvalidHandle),
        }
    }

    async fn receiver_for(
        &self,
        handle: StreamHandle,
    ) -> Result<Arc<Mutex<mpsc::Receiver<Vec<u8>>>>, StreamError> {
        let state = self.inner.lock().await;
        match state.handles.get(&handle.0) {
            Some(ChannelEnd::Receiver(rx)) => Ok(rx.clone()),
            Some(ChannelEnd::Sender(_)) => Err(StreamError::InvalidHandle),
            None => Err(StreamError::InvalidHandle),
        }
    }
}

impl Default for HttpStreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_pair_returns_distinct_ids() {
        let reg = HttpStreamRegistry::new();
        let (w, r) = reg.create_pair().await;
        assert_ne!(w.0, r.0);
        assert_eq!(reg.handle_count().await, 2);
    }

    #[tokio::test]
    async fn write_then_read_roundtrips_chunk() {
        let reg = HttpStreamRegistry::new();
        let (w, r) = reg.create_pair().await;
        let n = reg.write(w, b"hello".to_vec()).await.unwrap();
        assert_eq!(n, 5);
        let chunk = reg.read(r).await.unwrap();
        assert_eq!(&chunk, b"hello");
    }

    #[tokio::test]
    async fn write_into_receiver_handle_is_invalid() {
        let reg = HttpStreamRegistry::new();
        let (_w, r) = reg.create_pair().await;
        let err = reg.write(r, b"hi".to_vec()).await.unwrap_err();
        assert_eq!(err, StreamError::InvalidHandle);
    }

    #[tokio::test]
    async fn read_from_sender_handle_is_invalid() {
        let reg = HttpStreamRegistry::new();
        let (w, _r) = reg.create_pair().await;
        let err = reg.read(w).await.unwrap_err();
        assert_eq!(err, StreamError::InvalidHandle);
    }

    #[tokio::test]
    async fn try_write_returns_wouldblock_when_full() {
        let reg = HttpStreamRegistry::new();
        let (w, _r) = reg.create_pair().await;
        // Fill the channel to capacity.
        for _ in 0..STREAM_CHANNEL_CAPACITY {
            reg.try_write(w, vec![0u8; 8]).await.unwrap();
        }
        // Next try_write should WouldBlock.
        let err = reg.try_write(w, vec![0u8; 8]).await.unwrap_err();
        assert_eq!(err, StreamError::WouldBlock);
    }

    #[tokio::test]
    async fn close_sender_causes_receiver_eof() {
        let reg = HttpStreamRegistry::new();
        let (w, r) = reg.create_pair().await;
        reg.write(w, b"first".to_vec()).await.unwrap();
        reg.close(w).await.unwrap();
        // Drain buffered chunk then EOF.
        let first = reg.read(r).await.unwrap();
        assert_eq!(&first, b"first");
        let eof = reg.read(r).await.unwrap();
        assert!(eof.is_empty(), "expected EOF, got {:?}", eof);
    }

    #[tokio::test]
    async fn write_after_receiver_close_returns_closed() {
        let reg = HttpStreamRegistry::new();
        let (w, r) = reg.create_pair().await;
        reg.close(r).await.unwrap();
        let err = reg.write(w, b"x".to_vec()).await.unwrap_err();
        assert_eq!(err, StreamError::Closed);
    }

    #[tokio::test]
    async fn close_unknown_handle_errs() {
        let reg = HttpStreamRegistry::new();
        let err = reg.close(StreamHandle(9999)).await.unwrap_err();
        assert_eq!(err, StreamError::InvalidHandle);
    }

    #[tokio::test]
    async fn handle_ids_monotonic_within_registry() {
        let reg = HttpStreamRegistry::new();
        let (w1, r1) = reg.create_pair().await;
        let (w2, r2) = reg.create_pair().await;
        assert!(w1.0 < r1.0);
        assert!(r1.0 < w2.0);
        assert!(w2.0 < r2.0);
    }
}
