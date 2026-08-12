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
//! Capacity: 64 chunks × 16 KiB = 1 MiB per handle for SDK-originated
//! writes. Host-originated chunks can be larger; bounded reads split
//! those chunks across guest buffers while preserving stream order.
//!
//! ## Authority (ADR-0156 / ARN-207)
//!
//! Handle IDs are opaque and unguessable. Guest authority is **not** the
//! integer itself: `ProductionWasmHost` holds an invocation-scoped grant
//! table, and guest-facing ops require a grant. Kernel/bridge code uses
//! the privileged registry methods with the exact ends it created.

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

/// Chunk size used by SDK adapters when splitting writes. A single
/// SDK-originated chunk may be smaller (short writes), but never larger.
pub const STREAM_CHUNK_BYTES: usize = 16 * 1024;

/// Per-handle channel capacity in chunks. Total resident bytes per
/// handle = capacity × STREAM_CHUNK_BYTES. At 64 × 16 KiB this is
/// 1 MiB per handle, 2 MiB per bidirectional exchange.
pub const STREAM_CHANNEL_CAPACITY: usize = 64;

/// Maximum live handles in one process-global registry (DoS bound).
pub const MAX_STREAM_HANDLES_GLOBAL: usize = 16_384;

/// Maximum guest stream grants per invocation host (DoS bound).
pub const MAX_STREAM_GRANTS_PER_INVOCATION: usize = 64;

/// Opaque handle identifying one end of a streaming channel. Passed
/// from guest to host via FFI; host-side lookups go through
/// [`HttpStreamRegistry`].
///
/// The raw `u32` is **not** an authority-bearing capability (ADR-0156).
/// Guests may only operate on handles granted to their invocation.
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

/// Full set of endpoints the kernel dispatcher + guest share for one
/// inbound exchange (ADR-0069 dispatch + ADR-0057 Phase 2).
///
/// Mirror of [`OutboundExchange`] with the directions inverted:
///   * Request body: KERNEL writes (axum body chunks), GUEST reads.
///   * Response body: GUEST writes, KERNEL reads (to stream to axum).
///   * Head: GUEST sends (via `host_http_stream_send_response_head`),
///     KERNEL awaits via `await_inbound_response_head`.
pub struct InboundExchange {
    /// Guest-facing handle: guest reads request body here.
    pub guest_request_body: StreamHandle,
    /// Guest-facing handle: guest writes response body here.
    pub guest_response_body: StreamHandle,
    /// Kernel-facing handle: kernel writes axum body chunks here.
    pub kernel_request_body: StreamHandle,
    /// Kernel-facing handle: kernel reads response chunks here.
    pub kernel_response_body: StreamHandle,
    /// Kernel awaits the response head via the registry's
    /// `await_inbound_response_head(guest_response_body)` once
    /// the guest calls `submit_inbound_response_head(...)`.
    pub kernel_head_receiver_slot: StreamHandle,
}

/// Full set of endpoints the bridge task + guest share for one
/// outbound exchange.
///
/// Guest-facing pair (`guest_request_body`, `guest_response_body`)
/// becomes [`HttpStreamHandles`] returned by
/// `http_stream_begin_outbound`. The bridge task owns the other
/// three pieces: reading request chunks from `bridge_request_body`,
/// sending the response head via `bridge_head_sender`, then writing
/// response chunks into `bridge_response_body`.
pub struct OutboundExchange {
    pub guest_request_body: StreamHandle,
    pub guest_response_body: StreamHandle,
    pub bridge_request_body: StreamHandle,
    pub bridge_response_body: StreamHandle,
    pub bridge_head_sender: oneshot::Sender<HttpResponseHead>,
}

impl OutboundExchange {
    /// Guest-facing handles packaged for return from
    /// `http_stream_begin_outbound`.
    pub fn guest_handles(&self) -> HttpStreamHandles {
        HttpStreamHandles {
            request_body: self.guest_request_body,
            response_body: self.guest_response_body,
        }
    }
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
/// (and any other host that implements streaming). One registry may be
/// shared process-wide (`ServerState.http_stream_registry`); handle IDs
/// are opaque unguessable `u32`s and are **not** authority by themselves
/// (ADR-0156 / ARN-207).
pub struct HttpStreamRegistry {
    inner: Mutex<RegistryState>,
}

struct RegistryState {
    handles: BTreeMap<u32, ChannelEnd>,
    /// Outbound: oneshot receivers keyed on response-body handle ID.
    /// Bridge task holds the Sender and fires once the HTTP response
    /// head is received. Guest consumes via `await_response_head`.
    response_head_receivers: BTreeMap<u32, oneshot::Receiver<HttpResponseHead>>,
    /// Inbound: oneshot senders keyed on response-body handle ID.
    /// Guest fires the sender via `submit_inbound_response_head`
    /// once it has the head ready; kernel awaits the matching
    /// receiver via `await_inbound_response_head`.
    inbound_head_senders: BTreeMap<u32, oneshot::Sender<HttpResponseHead>>,
    inbound_head_receivers: BTreeMap<u32, oneshot::Receiver<HttpResponseHead>>,
    pending_reads: BTreeMap<u32, Vec<u8>>,
}

impl RegistryState {
    /// Allocate an unguessable free handle id. Not sequential — raw
    /// integers must not be enumerable capabilities (ARN-207).
    ///
    /// `// determinism-ok: production host I/O path; not SimActor scheduling.`
    fn alloc_id(&mut self) -> Result<u32, StreamError> {
        if self.handles.len() >= MAX_STREAM_HANDLES_GLOBAL {
            return Err(StreamError::Aborted(
                "stream handle budget exhausted".into(),
            ));
        }
        // determinism-ok: production host I/O path; not SimActor scheduling.
        // ~31 bits of unguessability (low bit forced 1 so id is never 0).
        // Grants remain the authority boundary; opacity is defense-in-depth.
        for _ in 0..64 {
            let id = (Uuid::new_v4().as_u128() as u32) | 1;
            if !self.handles.contains_key(&id) {
                return Ok(id);
            }
        }
        // Extremely unlikely: fall back to a linear probe from a fresh seed.
        // determinism-ok: production host I/O path; not SimActor scheduling.
        let mut probe = (Uuid::new_v4().as_u128() as u32) | 1;
        for _ in 0..1024 {
            if probe != 0 && !self.handles.contains_key(&probe) {
                return Ok(probe);
            }
            probe = probe.wrapping_add(1).max(1);
        }
        Err(StreamError::Aborted(
            "failed to allocate stream handle id".into(),
        ))
    }
}

impl HttpStreamRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryState {
                handles: BTreeMap::new(),
                response_head_receivers: BTreeMap::new(),
                inbound_head_senders: BTreeMap::new(),
                inbound_head_receivers: BTreeMap::new(),
                pending_reads: BTreeMap::new(),
            }),
        }
    }

    /// Allocate a new paired (sender, receiver) channel. Returns
    /// (write_handle, read_handle) — guests write into the first,
    /// the other end reads from the second.
    pub async fn create_pair(&self) -> Result<(StreamHandle, StreamHandle), StreamError> {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(STREAM_CHANNEL_CAPACITY);
        let mut state = self.inner.lock().await;
        // Need room for two ends.
        if state.handles.len() + 2 > MAX_STREAM_HANDLES_GLOBAL {
            return Err(StreamError::Aborted(
                "stream handle budget exhausted".into(),
            ));
        }
        let write_id = state.alloc_id()?;
        // Temporarily insert a placeholder so the second alloc cannot collide
        // with write_id (alloc_id only checks `handles`).
        state.handles.insert(write_id, ChannelEnd::Sender(tx));
        let read_id = match state.alloc_id() {
            Ok(id) => id,
            Err(e) => {
                state.handles.remove(&write_id);
                return Err(e);
            }
        };
        state
            .handles
            .insert(read_id, ChannelEnd::Receiver(Arc::new(Mutex::new(rx))));
        Ok((StreamHandle(write_id), StreamHandle(read_id)))
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
        self.read_bounded(handle, usize::MAX).await
    }

    /// Read up to `max_bytes` from the handle's receiver. If the next
    /// channel chunk is larger than the guest-provided buffer, return
    /// only the prefix and retain the remainder for subsequent reads.
    pub async fn read_bounded(
        &self,
        handle: StreamHandle,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StreamError> {
        if max_bytes == 0 {
            return Err(StreamError::Aborted(
                "read buffer capacity must be greater than zero".into(),
            ));
        }

        let rx_mutex = self.receiver_for(handle).await?;
        let mut rx = rx_mutex.lock().await;

        if let Some(chunk) = self.take_pending_read(handle, max_bytes).await {
            return Ok(chunk);
        }

        match rx.recv().await {
            Some(chunk) => Ok(self.split_for_bounded_read(handle, chunk, max_bytes).await),
            None => Ok(Vec::new()), // clean EOF
        }
    }

    /// Close a handle, freeing its registry slot. The other side of
    /// the channel observes this as EOF on its next read or a
    /// Closed error on its next write.
    pub async fn close(&self, handle: StreamHandle) -> Result<(), StreamError> {
        let mut state = self.inner.lock().await;
        state.pending_reads.remove(&handle.0);
        match state.handles.remove(&handle.0) {
            Some(_) => Ok(()),
            None => Err(StreamError::InvalidHandle),
        }
    }

    /// Open a full outbound exchange: request-body channel + response-
    /// body channel + response-head oneshot. Host bridge task gets
    /// (request_body_reader, response_body_writer, head_sender); guest
    /// gets (request_body_writer, response_body_reader). The head_sender
    /// is kept on the bridge task until the HTTP response arrives; the
    /// receiver is stored in the registry and handed to the guest on
    /// its first `await_response_head(response_body)` call.
    pub async fn open_outbound_exchange(&self) -> Result<OutboundExchange, StreamError> {
        let (req_writer, req_reader) = self.create_pair().await?;
        let (resp_writer, resp_reader) = match self.create_pair().await {
            Ok(pair) => pair,
            Err(e) => {
                // Best-effort cleanup of the first pair on budget failure.
                let _ = self.close(req_writer).await;
                let _ = self.close(req_reader).await;
                return Err(e);
            }
        };
        let (head_tx, head_rx) = oneshot::channel();
        {
            let mut state = self.inner.lock().await;
            state.response_head_receivers.insert(resp_reader.0, head_rx);
        }
        Ok(OutboundExchange {
            guest_request_body: req_writer,
            guest_response_body: resp_reader,
            bridge_request_body: req_reader,
            bridge_response_body: resp_writer,
            bridge_head_sender: head_tx,
        })
    }

    /// Await the response head for the given response-body handle.
    /// Returns once the bridge task has sent it, or an error if the
    /// handle is unknown / head is already taken / bridge dropped
    /// the sender without sending.
    pub async fn await_response_head(
        &self,
        response_body: StreamHandle,
    ) -> Result<HttpResponseHead, StreamError> {
        let rx = {
            let mut state = self.inner.lock().await;
            match state.response_head_receivers.remove(&response_body.0) {
                Some(rx) => rx,
                None => return Err(StreamError::InvalidHandle),
            }
        };
        rx.await
            .map_err(|_| StreamError::Aborted("response head sender dropped".into()))
    }

    /// Open a full inbound exchange for a single HTTP request
    /// dispatched via HttpEndpoint (ADR-0069). Kernel writes axum
    /// body chunks into `kernel_request_body`, guest reads them
    /// from `guest_request_body`. Guest writes its response body
    /// to `guest_response_body`; kernel streams chunks out via
    /// `kernel_response_body`. Guest fires the response head with
    /// `submit_inbound_response_head`; kernel awaits it via
    /// `await_inbound_response_head`.
    pub async fn open_inbound_exchange(&self) -> Result<InboundExchange, StreamError> {
        let (kern_req_writer, guest_req_reader) = self.create_pair().await?;
        let (guest_resp_writer, kern_resp_reader) = match self.create_pair().await {
            Ok(pair) => pair,
            Err(e) => {
                let _ = self.close(kern_req_writer).await;
                let _ = self.close(guest_req_reader).await;
                return Err(e);
            }
        };
        let (head_tx, head_rx) = oneshot::channel();
        {
            let mut state = self.inner.lock().await;
            state
                .inbound_head_senders
                .insert(guest_resp_writer.0, head_tx);
            state
                .inbound_head_receivers
                .insert(guest_resp_writer.0, head_rx);
        }
        Ok(InboundExchange {
            guest_request_body: guest_req_reader,
            guest_response_body: guest_resp_writer,
            kernel_request_body: kern_req_writer,
            kernel_response_body: kern_resp_reader,
            kernel_head_receiver_slot: guest_resp_writer,
        })
    }

    /// Close every listed handle that still exists. Used for RAII-style
    /// cleanup on timeout / cancel / request end (ARN-207).
    pub async fn close_handles(&self, handles: impl IntoIterator<Item = StreamHandle>) {
        for handle in handles {
            let _ = self.close(handle).await;
        }
    }

    /// Guest-called: submit the response head for an inbound
    /// exchange. Identified by the guest's response-body handle
    /// (the same handle it writes response chunks into).
    pub async fn submit_inbound_response_head(
        &self,
        guest_response_body: StreamHandle,
        head: HttpResponseHead,
    ) -> Result<(), StreamError> {
        let tx = {
            let mut state = self.inner.lock().await;
            match state.inbound_head_senders.remove(&guest_response_body.0) {
                Some(tx) => tx,
                None => return Err(StreamError::InvalidHandle),
            }
        };
        tx.send(head)
            .map_err(|_| StreamError::Aborted("kernel head receiver dropped".into()))
    }

    /// Kernel-called: await the head submitted by the guest.
    pub async fn await_inbound_response_head(
        &self,
        guest_response_body: StreamHandle,
    ) -> Result<HttpResponseHead, StreamError> {
        let rx = {
            let mut state = self.inner.lock().await;
            match state.inbound_head_receivers.remove(&guest_response_body.0) {
                Some(rx) => rx,
                None => return Err(StreamError::InvalidHandle),
            }
        };
        rx.await
            .map_err(|_| StreamError::Aborted("guest head sender dropped".into()))
    }

    /// Current handle count — for metrics and leak detection in tests.
    pub async fn handle_count(&self) -> usize {
        self.inner.lock().await.handles.len()
    }

    // --- Private helpers ---

    async fn sender_for(&self, handle: StreamHandle) -> Result<mpsc::Sender<Vec<u8>>, StreamError> {
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

    async fn take_pending_read(&self, handle: StreamHandle, max_bytes: usize) -> Option<Vec<u8>> {
        let mut state = self.inner.lock().await;
        let pending = state.pending_reads.remove(&handle.0)?;
        Some(Self::split_for_bounded_read_locked(
            &mut state, handle, pending, max_bytes,
        ))
    }

    async fn split_for_bounded_read(
        &self,
        handle: StreamHandle,
        chunk: Vec<u8>,
        max_bytes: usize,
    ) -> Vec<u8> {
        let mut state = self.inner.lock().await;
        Self::split_for_bounded_read_locked(&mut state, handle, chunk, max_bytes)
    }

    fn split_for_bounded_read_locked(
        state: &mut RegistryState,
        handle: StreamHandle,
        mut chunk: Vec<u8>,
        max_bytes: usize,
    ) -> Vec<u8> {
        if chunk.len() > max_bytes {
            let remainder = chunk.split_off(max_bytes);
            state.pending_reads.insert(handle.0, remainder);
        }
        chunk
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
        let (w, r) = reg.create_pair().await.unwrap();
        assert_ne!(w.0, r.0);
        assert_eq!(reg.handle_count().await, 2);
    }

    #[tokio::test]
    async fn write_then_read_roundtrips_chunk() {
        let reg = HttpStreamRegistry::new();
        let (w, r) = reg.create_pair().await.unwrap();
        let n = reg.write(w, b"hello".to_vec()).await.unwrap();
        assert_eq!(n, 5);
        let chunk = reg.read(r).await.unwrap();
        assert_eq!(&chunk, b"hello");
    }

    #[tokio::test]
    async fn bounded_read_splits_oversized_chunk_and_preserves_order() {
        let reg = HttpStreamRegistry::new();
        let (w, r) = reg.create_pair().await.unwrap();
        reg.write(w, b"abcdefghij".to_vec()).await.unwrap();
        reg.write(w, b"next".to_vec()).await.unwrap();

        let first = reg.read_bounded(r, 4).await.unwrap();
        let second = reg.read_bounded(r, 4).await.unwrap();
        let third = reg.read_bounded(r, 4).await.unwrap();
        let fourth = reg.read_bounded(r, 4).await.unwrap();

        assert_eq!(&first, b"abcd");
        assert_eq!(&second, b"efgh");
        assert_eq!(&third, b"ij");
        assert_eq!(&fourth, b"next");
    }

    #[tokio::test]
    async fn write_into_receiver_handle_is_invalid() {
        let reg = HttpStreamRegistry::new();
        let (_w, r) = reg.create_pair().await.unwrap();
        let err = reg.write(r, b"hi".to_vec()).await.unwrap_err();
        assert_eq!(err, StreamError::InvalidHandle);
    }

    #[tokio::test]
    async fn read_from_sender_handle_is_invalid() {
        let reg = HttpStreamRegistry::new();
        let (w, _r) = reg.create_pair().await.unwrap();
        let err = reg.read(w).await.unwrap_err();
        assert_eq!(err, StreamError::InvalidHandle);
    }

    #[tokio::test]
    async fn try_write_returns_wouldblock_when_full() {
        let reg = HttpStreamRegistry::new();
        let (w, _r) = reg.create_pair().await.unwrap();
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
        let (w, r) = reg.create_pair().await.unwrap();
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
        let (w, r) = reg.create_pair().await.unwrap();
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
    async fn inbound_exchange_roundtrips_head_and_body() {
        let reg = HttpStreamRegistry::new();
        let exchange = reg.open_inbound_exchange().await.unwrap();

        // Kernel pushes request body.
        reg.write(
            exchange.kernel_request_body,
            b"git-upload-pack-request-body".to_vec(),
        )
        .await
        .unwrap();
        reg.close(exchange.kernel_request_body).await.unwrap();

        // Guest reads request, produces response body + head.
        let chunk = reg.read(exchange.guest_request_body).await.unwrap();
        assert_eq!(&chunk, b"git-upload-pack-request-body");
        let eof = reg.read(exchange.guest_request_body).await.unwrap();
        assert!(eof.is_empty());

        // Guest submits head.
        let head = HttpResponseHead {
            status: 200,
            headers: vec![(
                "content-type".into(),
                "application/x-git-upload-pack-result".into(),
            )],
        };
        reg.submit_inbound_response_head(exchange.guest_response_body, head.clone())
            .await
            .unwrap();

        // Guest writes response body and closes.
        reg.write(exchange.guest_response_body, b"packfile-bytes".to_vec())
            .await
            .unwrap();
        reg.close(exchange.guest_response_body).await.unwrap();

        // Kernel awaits head, then drains response body.
        let kernel_head = reg
            .await_inbound_response_head(exchange.kernel_head_receiver_slot)
            .await
            .unwrap();
        assert_eq!(kernel_head.status, head.status);
        assert_eq!(kernel_head.headers, head.headers);
        let body_chunk = reg.read(exchange.kernel_response_body).await.unwrap();
        assert_eq!(&body_chunk, b"packfile-bytes");
        let eof2 = reg.read(exchange.kernel_response_body).await.unwrap();
        assert!(eof2.is_empty());
    }

    #[tokio::test]
    async fn inbound_submit_head_unknown_handle() {
        let reg = HttpStreamRegistry::new();
        let head = HttpResponseHead::default();
        let err = reg
            .submit_inbound_response_head(StreamHandle(9999), head)
            .await
            .unwrap_err();
        assert_eq!(err, StreamError::InvalidHandle);
    }

    #[tokio::test]
    async fn handle_ids_are_unique_and_non_zero() {
        let reg = HttpStreamRegistry::new();
        let (w1, r1) = reg.create_pair().await.unwrap();
        let (w2, r2) = reg.create_pair().await.unwrap();
        let ids = [w1.0, r1.0, w2.0, r2.0];
        assert!(ids.iter().all(|&id| id != 0));
        let set: std::collections::BTreeSet<_> = ids.into_iter().collect();
        assert_eq!(set.len(), 4, "handle ids must be unique");
    }

    #[tokio::test]
    async fn handle_ids_are_not_dense_low_integers() {
        // Sequential allocation made 1..=N guessable. Opaque IDs must not
        // land in a tiny low range that enumeration can cover cheaply.
        let reg = HttpStreamRegistry::new();
        let (w, r) = reg.create_pair().await.unwrap();
        // Probabilistically: two random u32s both < 256 is vanishingly rare.
        // If both are small, something regressed to sequential allocation.
        let both_tiny = w.0 < 256 && r.0 < 256;
        assert!(
            !both_tiny,
            "expected unguessable handle ids, got w={} r={}",
            w.0, r.0
        );
    }
}
