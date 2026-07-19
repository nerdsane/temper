use serde::{Deserialize, Serialize};

/// Event type used for the parent-journal record of a Composite action.
///
/// Concrete sub-write events remain the state-changing events on their target
/// journals. This event records the composite intent and the exact sub-write
/// journals/idempotency keys that were committed atomically with it.
pub const COMPOSITE_EVENT_TYPE: &str = "CompositeEvent";

/// Replay/audit record for one Composite action application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeEvent {
    pub tenant: String,
    pub parent_entity_type: String,
    pub parent_entity_id: String,
    pub parent_action: String,
    pub composite_idempotency_key: String,
    pub sub_writes: Vec<CompositeEventSubWrite>,
}

/// One concrete sub-write recorded in a [`CompositeEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeEventSubWrite {
    pub index: usize,
    pub entity_type: String,
    pub entity_id: String,
    pub action: String,
    pub idempotency_key: String,
}

/// Marker trait for domain events.
/// Events must be serializable (for persistence) and Send + 'static (for async).
pub trait DomainEvent:
    Send + Serialize + for<'de> Deserialize<'de> + std::fmt::Debug + 'static
{
}

/// Metadata attached to every persisted event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMetadata {
    /// Unique ID of this event.
    pub event_id: uuid::Uuid,
    /// ID of the command/message that caused this event.
    pub causation_id: uuid::Uuid,
    /// Correlation ID for tracing across actor boundaries.
    pub correlation_id: uuid::Uuid,
    /// Timestamp of persistence.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Actor that produced this event.
    pub actor_id: String,
}

/// Trait for event-sourced persistent actors.
/// Extends the base Actor trait with event journal and snapshot capabilities.
///
/// The persistence protocol:
/// 1. Actor receives command (message)
/// 2. Handler validates command against current state
/// 3. Handler produces events via ctx.persist(event)
/// 4. Events are written to journal (Postgres)
/// 5. Events are applied to state via apply_event()
/// 6. Periodically, state is snapshotted for fast recovery
///
/// On restart:
/// 1. Load latest snapshot (if any)
/// 2. Replay events since snapshot
/// 3. Actor state is rebuilt — ready to process messages
pub trait PersistentActor: Send + 'static {
    type Event: DomainEvent;
    type State: Send + Serialize + for<'de> Deserialize<'de> + 'static;

    /// The persistence ID. Must be unique across the system.
    /// Typically: "{entity_type}:{entity_id}"
    fn persistence_id(&self) -> &str;

    /// Apply a single event to the state. Must be pure (no side effects).
    /// This is called during replay and during live operation.
    fn apply_event(state: &mut Self::State, event: &Self::Event);

    /// How often to snapshot (every N events). Default: every 100 events.
    fn snapshot_every(&self) -> u64 {
        100
    }
}

/// A declared-key row to co-commit with an append (ADR-0153). The entity claims
/// `key_hash` for `key_name`; the store writes it into `entity_key_index` in the
/// same transaction as the journal append, giving the read plane an `O(log n)`
/// present/absent probe (the negative-existence access path, ARN-68).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityKeyRow {
    /// The declared key's identifier (the `[[key]]` block's `name`).
    pub key_name: String,
    /// The canonical, type-tagged hash of the key's values.
    pub key_hash: String,
}

/// A derived vector-index row to co-commit with an append (ADR-0155). Parsed from
/// the entity's post-transition state for one declared `[[vector]]` path: the
/// float vector and the model tag that partitions its space. Stores that maintain
/// `entity_vector_index` write one row per `(decl_name, model_tag, entity_id)`; the
/// blob is packed little-endian f32. Unlike a key row this has no uniqueness
/// constraint — it is derived, rebuildable ranking state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityVectorRow {
    /// The declared vector path's identifier (the `[[vector]]` block's `name`).
    pub decl_name: String,
    /// The model tag that partitions this vector's space (only same-tag vectors
    /// are ever compared).
    pub model_tag: String,
    /// The float vector, exactly `dims` long.
    pub vector: Vec<f32>,
}

/// Pack an `f32` slice to little-endian bytes — the `entity_vector_index` blob
/// encoding shared by every backend (ADR-0155). Kept here beside [`EntityVectorRow`]
/// so the stores and the kernel ranking agree on the byte layout.
pub fn pack_f32_le(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Unpack little-endian bytes back to `f32`. `None` if the byte length is not a
/// multiple of 4, or if any component is not finite (both signal a corrupt blob),
/// so a bad row is skipped rather than panicking or feeding a `NaN`/`inf` into the
/// kNN ranking — where a `NaN` would sort ahead of every real score.
pub fn unpack_f32_le(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return None;
        }
        out.push(value);
    }
    Some(out)
}

/// One candidate row returned from the vector index for a kNN read (ADR-0155):
/// an entity and its packed vector for one `(tenant, type, decl, model_tag)`
/// partition. The kernel — not the store — computes the metric over these in the
/// store-supplied (entity-id) order, so ranking is identical across backends.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityVectorCandidate {
    /// The entity holding this vector.
    pub entity_id: String,
    /// The float vector, exactly `dims` long.
    pub vector: Vec<f32>,
}

/// Which derived index families must be reconciled to the exact rows supplied
/// with an event append.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexReconciliation {
    /// Replace all declared-key rows for the entity, including with an empty set.
    pub keys: bool,
    /// Replace all vector rows for the entity, including with an empty set.
    pub vectors: bool,
}

/// Opaque, RAII guard that serializes one type-wide projection reconciliation
/// against other reconcilers and against live projection-maintaining writes.
///
/// Stores with authoritative derived projections keep their backend-specific
/// guard (for example a PostgreSQL advisory-lock transaction or a simulation
/// write-lock guard) inside this value. Dropping it releases the fence. Stores
/// without such projections may use the default no-op guard.
pub struct ProjectionReconciliationFence {
    _guard: Box<dyn std::any::Any + Send>,
}

impl ProjectionReconciliationFence {
    /// Wrap a backend-specific owned guard for RAII release.
    pub fn new<T>(guard: T) -> Self
    where
        T: std::any::Any + Send,
    {
        Self {
            _guard: Box::new(guard),
        }
    }
}

impl std::fmt::Debug for ProjectionReconciliationFence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionReconciliationFence")
            .finish_non_exhaustive()
    }
}

/// Trait for the event store backend (implemented by temper-store-postgres).
/// Uses desugared async-in-trait to enforce Send bounds on futures.
pub trait EventStore: Send + Sync + 'static {
    /// Append events to the journal.
    fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send;

    /// Append events and co-commit declared key-index rows (ADR-0153) in the
    /// **same transaction** as the journal append. A thin forwarder to
    /// [`EventStore::append_with_index_rows`] with no vector rows and no vector
    /// reconcile, so callers that only maintain keys are unchanged. The co-commit
    /// logic lives in `append_with_index_rows`, which query-plane backends override.
    fn append_with_keys(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[EntityKeyRow],
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        self.append_with_index_rows(
            persistence_id,
            expected_sequence,
            events,
            key_rows,
            &[],
            IndexReconciliation {
                keys: true,
                vectors: false,
            },
        )
    }

    /// Append events and co-commit BOTH declared key-index rows (ADR-0153) and
    /// derived vector-index rows (ADR-0155) in the **same transaction** as the
    /// journal append. This is the single co-commit entry point the entity actor
    /// calls. `reconcile_keys` means `key_rows` is the entity's exact current
    /// declared-key set: the store deletes every prior row for that entity before
    /// inserting the provided rows, including when the set is empty. The default
    /// ignores the index kinds and delegates to
    /// [`EventStore::append`] — stores with a query plane that co-commit (postgres,
    /// sim) override it; Turso also overrides it to maintain the vector index
    /// write-behind (event first, index follows). When `reconcile_vectors` is true
    /// (the entity's type declares ≥1 `[[vector]]` path) the store first DELETES all
    /// of the entity's vector rows, then inserts `vector_rows` — so a delete
    /// transition or a cleared vector/model property purges the stale rows instead of
    /// leaving them to be ranked forever. The sequence and atomicity contract is
    /// identical to `append`.
    fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconciliation: IndexReconciliation,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        let _ = (key_rows, vector_rows, reconciliation);
        self.append(persistence_id, expected_sequence, events)
    }

    /// Acquire the exclusive reconciliation fence for one `(tenant,
    /// entity_type)` projection partition.
    ///
    /// Authoritative projection stores override this with a distributed or
    /// deterministic fence. Their live [`EventStore::append_with_index_rows`]
    /// implementation must take the matching shared fence whenever it maintains
    /// keys or vectors. The caller holds the returned guard from the definitive
    /// watermark read through exact reconciliation and watermark commit, so a
    /// second worker cannot act on stale coverage and a live write cannot be
    /// overwritten by replayed state. The default is a no-op for stores without
    /// authoritative derived projections.
    fn acquire_projection_reconciliation_fence(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<ProjectionReconciliationFence, PersistenceError>> + Send
    {
        let _ = (tenant, entity_type);
        async { Ok(ProjectionReconciliationFence::new(())) }
    }

    /// Acquire the shared side of the `(tenant, entity_type)` projection fence.
    ///
    /// Indexed readers hold this from before checking their authority watermark
    /// through the corresponding key lookup or vector scan. This prevents an
    /// authoritative read from observing the destructive middle of an exact
    /// purge-and-rebuild. The default is a no-op for stores without authoritative
    /// derived projections.
    fn acquire_projection_read_fence(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<ProjectionReconciliationFence, PersistenceError>> + Send
    {
        let _ = (tenant, entity_type);
        async { Ok(ProjectionReconciliationFence::new(())) }
    }

    /// Whether this store maintains declared-key rows on every live write and can
    /// therefore make a completed key watermark authoritative for absence.
    ///
    /// The default is false. Stores must opt in together with implementations of
    /// key reconciliation, durable watermarks, and live key co-commit.
    fn has_authoritative_key_index(&self) -> bool {
        false
    }

    /// Whether this store can persist and safely reuse a vector reconciliation
    /// watermark across restarts.
    ///
    /// Backends whose vector maintenance is write-behind may return false and
    /// replay/reconcile on every startup. This preserves correctness after an
    /// exhausted write-behind retry without treating an old watermark as current.
    fn has_durable_vector_backfill_watermark(&self) -> bool {
        false
    }

    /// Reconcile the derived vector-index rows for an **existing** entity to exactly
    /// `vector_rows` (ADR-0155), without appending a journal event: DELETE every
    /// existing row for `(tenant, entity_type, entity_id)`, then INSERT `vector_rows`.
    /// Idempotent, and an empty `vector_rows` PURGES the entity (used to clean up a
    /// deleted or un-embedded entity). Used by the backfill and by the Turso
    /// write-behind path. `source_sequence` is the journal sequence from which the
    /// rows were derived; write-behind stores use it to reject stale replay after a
    /// newer append. The default is a no-op (non-indexing backends); query-plane stores
    /// implement it.
    fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        source_sequence: u64,
        vector_rows: &[EntityVectorRow],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, entity_id, source_sequence, vector_rows);
        async { Ok(()) }
    }

    /// The candidate `(entity_id, vector)` rows for one vector-index partition
    /// `(tenant, entity_type, decl_name, model_tag)`, in **deterministic entity-id
    /// order** (ADR-0155), capped at `limit` rows. The kernel ranks these; the store
    /// only supplies the packed vectors, and applies `LIMIT` so an over-budget
    /// partition is detected (caller passes `budget + 1`) without loading the whole
    /// partition into memory. Default empty (non-indexing backends have no index).
    fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<EntityVectorCandidate>, PersistenceError>> + Send
    {
        let _ = (tenant, entity_type, decl_name, model_tag, limit);
        async { Ok(Vec::new()) }
    }

    /// Record that `entity_vector_index` is **complete** for `(tenant, entity_type)`
    /// under the versioned `vector_set` signature (ADR-0155, mirroring
    /// `mark_key_index_backfilled`). Declaration or reconciliation-schema changes
    /// produce a different signature and force an exact re-index. Idempotent. The
    /// default fails closed; only stores that return true from
    /// [`EventStore::has_durable_vector_backfill_watermark`] may persist one.
    fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, vector_set);
        async {
            Err(PersistenceError::Storage(
                "durable vector backfill watermark is unsupported".to_string(),
            ))
        }
    }

    /// The `(entity_type, vector_set)` watermarks for `tenant` — each type whose
    /// `entity_vector_index` backfill is complete, paired with the covered path set.
    /// Default empty (no backend authority). Mirrors `key_index_backfilled_types`.
    fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        let _ = tenant;
        async { Ok(Vec::new()) }
    }

    /// The `entity_id`s that have at least one `entity_vector_index` row for
    /// `(tenant, entity_type)`. Exact reconciliation unions these IDs with durable
    /// entity enumeration so deleted or projection-only rows remain discoverable and
    /// can be purged. Default empty (no projection enumeration). Mirrors
    /// `keyed_entity_ids_for_type`.
    fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        let _ = (tenant, entity_type);
        async { Ok(Vec::new()) }
    }

    /// Reconcile an entity's complete declared key-index row set (ADR-0153) without
    /// appending a journal event. Implementations replace every existing row for the
    /// entity with `key_rows`; an empty slice therefore purges deleted, phantom, or
    /// no-longer-keyable entities. Idempotent: re-running yields the same rows. The
    /// default is a no-op for non-indexing backends.
    fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[EntityKeyRow],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, entity_id, key_rows);
        async { Ok(()) }
    }

    /// Resolve an entity by a declared key (ADR-0153): the `entity_id` currently
    /// holding `(key_name, key_hash)`, or `None` if absent. This is the
    /// negative-existence access path — present *and* absent in one `O(log n)`
    /// probe, no scan. Default returns `None` (non-indexing backends); the
    /// query-plane stores override it against `entity_key_index`.
    fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, PersistenceError>> + Send {
        let _ = (tenant, entity_type, key_name, key_hash);
        async { Ok(None) }
    }

    /// Record that `entity_key_index` is **complete** for `(tenant, entity_type)`
    /// — every existing entity of that type has been keyed by the backfill
    /// (ADR-0153 watermark). Once set, a keyed read MISS is authoritative absence,
    /// which retires the full-type reconcile scan (#324) for that type: the read
    /// plane can answer "not found" without scanning. Idempotent.
    ///
    /// **Soundness invariant — only override this on a backend that co-commits key
    /// rows on EVERY write** (i.e. overrides [`EventStore::append_with_keys`]). The
    /// watermark asserts the index is complete *and stays complete*; a backend that
    /// backfills but does not maintain keys live (e.g. Turso, which does not
    /// co-commit) would let a later write go unkeyed, and a keyed miss for that
    /// present entity would then read as authoritative absence — a silent
    /// correctness bug. Such backends MUST keep the default failure so they never
    /// become authoritative (their keyed misses fall back to the scan — correct,
    /// just not bounded). Postgres co-commits and overrides this; the sim store does
    /// too for DST. The default fails closed.
    ///
    /// `key_set` is the versioned, deterministic declared-key signature the backfill
    /// just covered. A declaration or reconciliation-schema change produces a new
    /// signature and forces the type to be reconciled again.
    fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, key_set);
        async {
            Err(PersistenceError::Storage(
                "authoritative key-index watermark is unsupported".to_string(),
            ))
        }
    }

    /// The `(entity_type, key_set)` watermarks for `tenant` — each type whose
    /// `entity_key_index` reconciliation is complete, paired with the versioned
    /// declared-key signature it covered. A keyed miss is authoritative only when the
    /// stored signature equals the current one. Default empty (scan-safe).
    fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        let _ = tenant;
        async { Ok(Vec::new()) }
    }

    /// The `entity_id`s that have at least one `entity_key_index` row for
    /// `(tenant, entity_type)`. Exact reconciliation unions these IDs with durable
    /// entity enumeration so deleted or projection-only rows remain discoverable and
    /// can be purged. Default empty (no projection enumeration).
    fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        let _ = (tenant, entity_type);
        async { Ok(Vec::new()) }
    }

    /// Atomically append events and their exact declared projection rows to
    /// multiple journals.
    ///
    /// Backends must either commit every append in `appends`, or commit none.
    /// Stores that maintain keys or vectors must co-commit each item's rows and
    /// take the same per-type shared reconciliation fence as
    /// [`EventStore::append_with_index_rows`]. This is the storage primitive
    /// composite actions use for cross-actor writes.
    fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceAppendResult>, PersistenceError>> + Send;

    /// Read the complete ordered journal tail after the given sequence number.
    ///
    /// A backend that cannot deliver the complete tail visible to this read,
    /// including one that detects a truncated or partial response, must return
    /// an error rather than a successful prefix.
    fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceEnvelope>, PersistenceError>> + Send;

    /// Save a state snapshot.
    fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send;

    /// Load the latest snapshot.
    fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<(u64, Vec<u8>)>, PersistenceError>> + Send;

    /// List all distinct `(entity_type, entity_id)` pairs for a tenant.
    fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send;

    /// List distinct entity IDs for one `(tenant, entity_type)` pair.
    fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send;

    /// List at most `limit` authoritative `(entity_type, entity_id)` pairs for
    /// a tenant, optionally scoped to one entity type.
    ///
    /// Storage backends should override this to apply the bound inside the
    /// backing query. The default is intended for small in-memory/test stores.
    fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        async move {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let mut entities = if let Some(entity_type) = entity_type {
                self.list_entity_ids_by_type(tenant, entity_type)
                    .await?
                    .into_iter()
                    .map(|entity_id| (entity_type.to_string(), entity_id))
                    .collect::<Vec<_>>()
            } else {
                self.list_entity_ids(tenant).await?
            };
            entities.sort();
            entities.truncate(limit);
            Ok(entities)
        }
    }
}

/// A persisted event with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceEnvelope {
    /// Monotonic sequence number within the entity's journal.
    pub sequence_nr: u64,
    /// Fully qualified event type name.
    pub event_type: String,
    /// Serialized event payload.
    pub payload: serde_json::Value,
    /// Event metadata (causation, correlation, timestamp).
    pub metadata: EventMetadata,
}

/// One stream append inside an atomic multi-journal append.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceAppend {
    /// Persistence ID in the form `{tenant}:{entity_type}:{entity_id}`.
    pub persistence_id: String,
    /// Optimistic-concurrency sequence expected before this append.
    pub expected_sequence: u64,
    /// Events to append to this journal.
    pub events: Vec<PersistenceEnvelope>,
    /// Exact current declared-key rows for this entity.
    #[serde(default)]
    pub key_rows: Vec<EntityKeyRow>,
    /// Exact current declared-vector rows for this entity.
    #[serde(default)]
    pub vector_rows: Vec<EntityVectorRow>,
    /// Projection families that must be exactly reconciled with this append.
    #[serde(default)]
    pub reconciliation: IndexReconciliation,
}

/// New sequence number for one stream after an atomic batch append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceAppendResult {
    /// Persistence ID that was appended.
    pub persistence_id: String,
    /// New highest sequence number for this journal.
    pub sequence_nr: u64,
}

/// Errors that can occur during event persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// Optimistic concurrency check failed (another writer appended first).
    #[error("optimistic concurrency violation: expected sequence {expected}, got {actual}")]
    ConcurrencyViolation { expected: u64, actual: u64 },

    /// Event serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Underlying storage backend returned an error.
    #[error("storage error: {0}")]
    Storage(String),
}

/// Convert backend-specific errors into [`PersistenceError::Storage`].
pub fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}
