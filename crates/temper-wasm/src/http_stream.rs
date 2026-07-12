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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, oneshot};

/// Chunk size used by SDK adapters when splitting writes. A single
/// SDK-originated chunk may be smaller (short writes), but never larger.
pub const STREAM_CHUNK_BYTES: usize = 16 * 1024;

/// Per-handle channel capacity in chunks. Total resident bytes per
/// handle = capacity × STREAM_CHUNK_BYTES. At 64 × 16 KiB this is
/// 1 MiB per handle, 2 MiB per bidirectional exchange.
pub const STREAM_CHANNEL_CAPACITY: usize = 64;

/// Opaque handle identifying one end of a streaming channel. Passed
/// from guest to host via FFI; host-side lookups go through
/// [`HttpStreamRegistry`].
///
/// The `u32` is only an index. Authority comes from the [`StreamScope`]
/// the handle is bound to (ADR-0156): a guest may only operate on a
/// handle whose owning scope matches the guest's ambient scope, so a
/// guessed integer from another invocation resolves to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamHandle(pub u32);

/// Per-invocation ownership scope for stream handles (ADR-0156).
///
/// Minted once per WASM invocation and carried ambiently on the host;
/// it never crosses the guest FFI boundary. Every handle created for an
/// invocation is bound to its scope, and guest read/write/close ops must
/// present the owning scope. Because the guest never names its scope, it
/// cannot forge another invocation's — making the guessable handle index
/// non-authoritative on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamScope(pub u64);

impl StreamScope {
    /// Scope used by hosts that own a private (non-shared) registry.
    /// Safe as a constant because such a registry is never shared across
    /// invocations, so its handles are unreachable from any other host.
    /// Shared-registry hosts always receive a unique minted scope.
    pub const PRIVATE: StreamScope = StreamScope(1);
}

/// Maximum concurrent outbound streaming exchanges a single invocation
/// may open (ADR-0156). Bounds buffered bytes per invocation together
/// with the per-handle channel capacity. Inbound exchanges are minted
/// one-per-request by the kernel and are not counted here.
pub const MAX_OUTBOUND_STREAMS_PER_SCOPE: usize = 64;

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

/// A registry slot: one channel end plus the authority metadata that
/// governs who may operate on it (ADR-0156).
struct HandleEntry {
    end: ChannelEnd,
    /// The invocation that owns this handle. Guest ops must present it.
    scope: StreamScope,
    /// Whether this handle was handed to the guest. Kernel/bridge-facing
    /// handles are `false` and are unreachable from the guest FFI even
    /// within the guest's own scope.
    guest_facing: bool,
}

/// Registry of active stream handles. Lives on `ProductionWasmHost`
/// (and any other host that implements streaming) and may be shared
/// between the ADR-0069 dispatcher and a per-request host. Handle IDs
/// are indices; authority is the owning [`StreamScope`], so sharing the
/// registry never lets one invocation reach another's handles.
pub struct HttpStreamRegistry {
    inner: Mutex<RegistryState>,
}

struct RegistryState {
    next_id: u32,
    next_scope: u64,
    handles: BTreeMap<u32, HandleEntry>,
    /// Every handle / head-channel id created under a scope, so
    /// [`HttpStreamRegistry::close_scope`] can reclaim an invocation's
    /// entire footprint in one call. Ids are never reused, so an id
    /// belongs to exactly one scope for the process lifetime.
    scope_ids: BTreeMap<StreamScope, BTreeSet<u32>>,
    /// Currently-open outbound exchanges per scope, keyed by their
    /// guest response-body id, for the per-scope *concurrency* bound
    /// (ADR-0156 Sub-Decision 4). An entry is removed when the guest
    /// closes that exchange, so sequential outbound calls never exhaust
    /// the budget — only genuinely concurrent ones count.
    outbound_open: BTreeMap<StreamScope, BTreeSet<u32>>,
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
    /// Record that `id` belongs to `scope` for later `close_scope`.
    fn tag_scope(&mut self, scope: StreamScope, id: u32) {
        self.scope_ids.entry(scope).or_default().insert(id);
    }
}

impl HttpStreamRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryState {
                next_id: 1,
                // Scope 1 is reserved for `StreamScope::PRIVATE`; minted
                // scopes start at 2 so a shared-registry host never
                // collides with a private-registry host.
                next_scope: 2,
                handles: BTreeMap::new(),
                scope_ids: BTreeMap::new(),
                outbound_open: BTreeMap::new(),
                response_head_receivers: BTreeMap::new(),
                inbound_head_senders: BTreeMap::new(),
                inbound_head_receivers: BTreeMap::new(),
                pending_reads: BTreeMap::new(),
            }),
        }
    }

    /// Mint a fresh, unique [`StreamScope`] for one invocation. The
    /// caller binds every handle it opens to this scope and carries it
    /// ambiently on the host; it must never cross the guest FFI boundary.
    pub async fn mint_scope(&self) -> StreamScope {
        let mut state = self.inner.lock().await;
        let scope = StreamScope(state.next_scope);
        state.next_scope += 1;
        scope
    }

    /// Allocate a new paired (sender, receiver) channel bound to
    /// `scope`. `write_guest_facing`/`read_guest_facing` mark which end
    /// (if any) is handed to the guest.
    async fn create_pair_scoped(
        &self,
        scope: StreamScope,
        write_guest_facing: bool,
        read_guest_facing: bool,
    ) -> (StreamHandle, StreamHandle) {
        let mut state = self.inner.lock().await;
        Self::create_pair_locked(&mut state, scope, write_guest_facing, read_guest_facing)
    }

    /// Allocate a scoped pair against an already-held registry lock, so a
    /// caller can create channels and update budgets atomically.
    fn create_pair_locked(
        state: &mut RegistryState,
        scope: StreamScope,
        write_guest_facing: bool,
        read_guest_facing: bool,
    ) -> (StreamHandle, StreamHandle) {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(STREAM_CHANNEL_CAPACITY);
        let write_id = state.next_id;
        state.next_id += 1;
        let read_id = state.next_id;
        state.next_id += 1;
        state.handles.insert(
            write_id,
            HandleEntry {
                end: ChannelEnd::Sender(tx),
                scope,
                guest_facing: write_guest_facing,
            },
        );
        state.handles.insert(
            read_id,
            HandleEntry {
                end: ChannelEnd::Receiver(Arc::new(Mutex::new(rx))),
                scope,
                guest_facing: read_guest_facing,
            },
        );
        state.tag_scope(scope, write_id);
        state.tag_scope(scope, read_id);
        (StreamHandle(write_id), StreamHandle(read_id))
    }

    /// Allocate a new paired (sender, receiver) channel under a private
    /// scope with both ends guest-usable. Convenience for tests and
    /// callers that own an unshared registry.
    pub async fn create_pair(&self) -> (StreamHandle, StreamHandle) {
        self.create_pair_scoped(StreamScope::PRIVATE, true, true)
            .await
    }

    /// Verify that `scope` may operate on `handle` as a guest: the
    /// handle must exist, be owned by `scope`, and be guest-facing.
    /// Returns [`StreamError::InvalidHandle`] otherwise — the same error
    /// used for unknown handles, so ownership is never leaked.
    async fn authorize_guest(
        &self,
        scope: StreamScope,
        handle: StreamHandle,
    ) -> Result<(), StreamError> {
        let state = self.inner.lock().await;
        match state.handles.get(&handle.0) {
            Some(entry) if entry.scope == scope && entry.guest_facing => Ok(()),
            _ => Err(StreamError::InvalidHandle),
        }
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
            Some(entry) => {
                // Releasing an outbound exchange's response-body handle
                // frees a slot in its scope's concurrency budget.
                if let Some(open) = state.outbound_open.get_mut(&entry.scope) {
                    open.remove(&handle.0);
                }
                // The id is intentionally left in `scope_ids`: `close_scope`
                // relies on it to also reclaim any head channel still keyed
                // on this id that a guest never consumed. Every scope is
                // eventually `close_scope`d (per dispatch, or on host drop
                // for a private registry), so this does not grow unbounded.
                Ok(())
            }
            None => Err(StreamError::InvalidHandle),
        }
    }

    // --- Guest-checked operations (ADR-0156) ---
    //
    // These are the only stream ops reachable from the guest FFI. Each
    // verifies the guest's ambient `scope` owns a guest-facing `handle`
    // before delegating to the privileged op above. Kernel/bridge tasks
    // use the privileged ops directly.

    /// Guest read: authorize `scope` then read the next chunk.
    pub async fn read_as_guest(
        &self,
        scope: StreamScope,
        handle: StreamHandle,
    ) -> Result<Vec<u8>, StreamError> {
        self.authorize_guest(scope, handle).await?;
        self.read(handle).await
    }

    /// Guest bounded read: authorize `scope` then read up to `max_bytes`.
    pub async fn read_bounded_as_guest(
        &self,
        scope: StreamScope,
        handle: StreamHandle,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StreamError> {
        self.authorize_guest(scope, handle).await?;
        self.read_bounded(handle, max_bytes).await
    }

    /// Guest non-blocking write: authorize `scope` then write.
    pub async fn try_write_as_guest(
        &self,
        scope: StreamScope,
        handle: StreamHandle,
        chunk: Vec<u8>,
    ) -> Result<usize, StreamError> {
        self.authorize_guest(scope, handle).await?;
        self.try_write(handle, chunk).await
    }

    /// Guest close: authorize `scope` then close.
    pub async fn close_as_guest(
        &self,
        scope: StreamScope,
        handle: StreamHandle,
    ) -> Result<(), StreamError> {
        self.authorize_guest(scope, handle).await?;
        self.close(handle).await
    }

    /// Guest await of an outbound response head: authorize `scope` then
    /// consume the head receiver.
    pub async fn await_response_head_as_guest(
        &self,
        scope: StreamScope,
        response_body: StreamHandle,
    ) -> Result<HttpResponseHead, StreamError> {
        self.authorize_guest(scope, response_body).await?;
        self.await_response_head(response_body).await
    }

    /// Guest submit of an inbound response head: authorize `scope` then
    /// fire the head sender.
    pub async fn submit_inbound_response_head_as_guest(
        &self,
        scope: StreamScope,
        guest_response_body: StreamHandle,
        head: HttpResponseHead,
    ) -> Result<(), StreamError> {
        self.authorize_guest(scope, guest_response_body).await?;
        self.submit_inbound_response_head(guest_response_body, head)
            .await
    }

    /// Reclaim every handle, pending read, and head channel owned by
    /// `scope` (ADR-0156 Sub-Decision 3). Dropping the channels signals
    /// EOF/abort to any peer and unblocks pump/bridge tasks. Idempotent.
    pub async fn close_scope(&self, scope: StreamScope) {
        let mut state = self.inner.lock().await;
        Self::close_scope_locked(&mut state, scope);
    }

    fn close_scope_locked(state: &mut RegistryState, scope: StreamScope) {
        let Some(ids) = state.scope_ids.remove(&scope) else {
            return;
        };
        for id in ids {
            state.handles.remove(&id);
            state.pending_reads.remove(&id);
            state.response_head_receivers.remove(&id);
            state.inbound_head_senders.remove(&id);
            state.inbound_head_receivers.remove(&id);
        }
        state.outbound_open.remove(&scope);
    }

    /// Try to reclaim `scope` synchronously via a non-blocking lock.
    /// Returns `true` if the scope was reclaimed, `false` if the lock was
    /// contended (leaving the caller to retry asynchronously). Used by
    /// [`ScopeCleanupGuard`] on drop.
    pub fn try_close_scope(&self, scope: StreamScope) -> bool {
        match self.inner.try_lock() {
            Ok(mut state) => {
                Self::close_scope_locked(&mut state, scope);
                true
            }
            Err(_) => false,
        }
    }

    /// Open a full outbound exchange: request-body channel + response-
    /// body channel + response-head oneshot. Host bridge task gets
    /// (request_body_reader, response_body_writer, head_sender); guest
    /// gets (request_body_writer, response_body_reader). The head_sender
    /// is kept on the bridge task until the HTTP response arrives; the
    /// receiver is stored in the registry and handed to the guest on
    /// its first `await_response_head(response_body)` call.
    pub async fn open_outbound_exchange(
        &self,
        scope: StreamScope,
    ) -> Result<OutboundExchange, StreamError> {
        let (head_tx, head_rx) = oneshot::channel();
        // Budget check, channel creation, and budget update happen under a
        // single lock so the concurrency bound can never be overshot by
        // two opens racing between a separate check and insert.
        let (req_writer, req_reader, resp_writer, resp_reader) = {
            let mut state = self.inner.lock().await;
            let open = state.outbound_open.get(&scope).map_or(0, BTreeSet::len);
            if open >= MAX_OUTBOUND_STREAMS_PER_SCOPE {
                return Err(StreamError::Aborted(format!(
                    "outbound stream budget exhausted ({MAX_OUTBOUND_STREAMS_PER_SCOPE} concurrent per invocation)"
                )));
            }
            // Request: guest writes (guest-facing), bridge reads.
            let (req_writer, req_reader) = Self::create_pair_locked(&mut state, scope, true, false);
            // Response: bridge writes, guest reads (guest-facing).
            let (resp_writer, resp_reader) =
                Self::create_pair_locked(&mut state, scope, false, true);
            state.response_head_receivers.insert(resp_reader.0, head_rx);
            // Track this exchange for the per-scope concurrency bound; the
            // slot is released when the guest closes `resp_reader`.
            state
                .outbound_open
                .entry(scope)
                .or_default()
                .insert(resp_reader.0);
            (req_writer, req_reader, resp_writer, resp_reader)
        };
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
    pub async fn open_inbound_exchange(&self, scope: StreamScope) -> InboundExchange {
        // Request: kernel writes, guest reads (guest-facing).
        let (kern_req_writer, guest_req_reader) = self.create_pair_scoped(scope, false, true).await;
        // Response: guest writes (guest-facing), kernel reads.
        let (guest_resp_writer, kern_resp_reader) =
            self.create_pair_scoped(scope, true, false).await;
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
        InboundExchange {
            guest_request_body: guest_req_reader,
            guest_response_body: guest_resp_writer,
            kernel_request_body: kern_req_writer,
            kernel_response_body: kern_resp_reader,
            kernel_head_receiver_slot: guest_resp_writer,
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
        match state.handles.get(&handle.0).map(|entry| &entry.end) {
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
        match state.handles.get(&handle.0).map(|entry| &entry.end) {
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

/// RAII guard that reclaims a [`StreamScope`]'s registry footprint when
/// dropped (ADR-0156 Sub-Decision 3). The inbound dispatcher holds one
/// for the life of an exchange — moved into the response body stream on
/// the success path — so a scope's streams are reclaimed on success,
/// error, timeout, and client disconnect alike.
pub struct ScopeCleanupGuard {
    registry: Arc<HttpStreamRegistry>,
    scope: StreamScope,
}

impl ScopeCleanupGuard {
    /// Create a guard that will close `scope` on `registry` when dropped.
    pub fn new(registry: Arc<HttpStreamRegistry>, scope: StreamScope) -> Self {
        Self { registry, scope }
    }
}

impl Drop for ScopeCleanupGuard {
    fn drop(&mut self) {
        // Fast path: reclaim synchronously if the registry lock is free
        // (the common case — the guard drops once its exchange is idle).
        if self.registry.try_close_scope(self.scope) {
            return;
        }
        // Contended: hand the reclaim to the async runtime so it still
        // happens rather than leaking. This keeps cleanup guaranteed even
        // when the guard drops while another task holds the lock (e.g. a
        // client disconnect racing an in-flight stream op).
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let registry = self.registry.clone();
                let scope = self.scope;
                handle.spawn(async move { registry.close_scope(scope).await });
            }
            Err(_) => {
                tracing::warn!(
                    scope = self.scope.0,
                    "stream scope cleanup skipped: registry contended and no runtime available"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "http_stream_test.rs"]
mod tests;
