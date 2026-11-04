use serde::{Deserialize, Serialize};

mod projection;
pub use projection::{JournalHead, ProjectionCheckpoint, ProjectionRepairCandidate};

mod catalog_group;
mod catalog_state;
mod secret;
mod tombstone;
pub use catalog_group::CatalogGroupRow;
pub use catalog_state::{catalog_state_for_storage, catalog_state_from_storage};
pub use secret::{SecretPrecondition, SecretWrite};
pub use tombstone::{EVENT_KIND_KEY, EventKind, TOMBSTONE_EVENT_TYPE, journal_is_deleted};

/// Event type used for the parent-journal record of a Composite action.
///
/// Concrete sub-write events remain the state-changing events on their target
/// journals. This event records the composite intent and the exact sub-write
/// journals/idempotency keys that were committed atomically with it.
pub const COMPOSITE_EVENT_TYPE: &str = "CompositeEvent";

/// How many WASM invocation log rows a tenant keeps, newest first. Its only
/// reader, `/observe/wasm` (module stats and invocation history), reads at
/// most the newest this many, so an older row is never read again; without a
/// bound the log grew by one row per integration call for as long as the
/// tenant ran.
pub const WASM_INVOCATIONS_KEPT_PER_TENANT: i64 = 10_000;

/// Rows one invocation-log delete removes while holding the shared writer.
/// A journal append waits for at most one batch, so a backlog drains in
/// batches rather than monopolizing the writer. A log is trimmed only once it
/// holds this many rows over [`WASM_INVOCATIONS_KEPT_PER_TENANT`], so a
/// delete runs once per this many inserts and never for a single row.
pub const WASM_INVOCATIONS_PRUNE_BATCH_ROWS: i64 = 100;

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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, PartialEq)]
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

/// One armed `[[state_timeout]]`, kept so that a restarted server arms it
/// again (ADR-0056). One row per entity: a state declares at most one timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTimerRecord {
    pub tenant: String,
    pub entity_type: String,
    pub entity_id: String,
    /// The state the timer was armed in.
    pub state: String,
    /// Names this arming, so a timer removes only its own row when it ends.
    pub arm_id: String,
    /// The entity's journal sequence when the timer was armed. A store keeps
    /// the row with the higher sequence, so a late write never undoes a re-arm.
    pub sequence_nr: u64,
    /// When the state's clock started (entry or latest `reset_on`), RFC 3339.
    pub started_at: String,
    /// When the timer falls due: `started_at` plus its state's budget, RFC
    /// 3339 in UTC, so text order is time order and a store can index it.
    /// None on a row a release without this column wrote, until a boot
    /// writes it.
    pub due_at: Option<String>,
    /// The `on_timeout` parameters, read when the timer was armed (JSON).
    pub params: String,
    /// The identity the timer fires as: the dispatch that armed it (JSON).
    pub armed_by: String,
}

/// A tenant's timer rows of one kind that are past their due time: a timer
/// that should have fired and has not, for whatever reason no process armed
/// or fired it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverdueTimers {
    /// How many rows are past due.
    pub count: u64,
    /// The earliest due time among them, RFC 3339. None when none is.
    pub oldest_due_at: Option<String>,
}

/// A tenant's timer rows past their due time: state timers and scheduled
/// actions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverdueTimerRows {
    /// Armed `[[state_timeout]]`s ([`StateTimerRecord`]).
    pub state_timers: OverdueTimers,
    /// Pending `schedule(...)` and `schedule_at(...)` actions
    /// ([`ScheduledActionRecord`]).
    pub scheduled_actions: OverdueTimers,
}

/// One pending `schedule(...)` or `schedule_at(...)` action, or the deadline
/// of a background module call that names an `on_failure`, kept so that a
/// restarted server arms it again. An entity can have many; each row is
/// removed in the commit of the event its action fires (for a call, the
/// call's answer), or after a fire that wrote no event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledActionRecord {
    pub tenant: String,
    pub entity_type: String,
    pub entity_id: String,
    /// Names this schedule: the arming event's journal sequence and the
    /// effect's place among that event's schedules (`<seq>.<index>`), or the
    /// called integration for a call's deadline (`<seq>.call:<integration>`),
    /// so a repeated write of the same arm lands on the same row.
    pub arm_id: String,
    /// The action to dispatch. Empty means durably cancelled: recovery only
    /// removes the row and must never dispatch it.
    pub action: String,
    /// When it is due, RFC 3339.
    pub due_at: String,
    /// The identity it fires as: the dispatch that armed it (JSON).
    pub armed_by: String,
}

/// The rows an append commits in the same transaction as its events.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommitRows<'a> {
    /// Declared key-index rows (ADR-0153).
    pub key_rows: &'a [EntityKeyRow],
    /// Derived vector-index rows (ADR-0155).
    pub vector_rows: &'a [EntityVectorRow],
    /// The type declares vector paths: the entity's vector rows become
    /// exactly `vector_rows`, so a delete or a cleared property purges them.
    pub reconcile_vectors: bool,
    /// The scheduled actions the appended transition armed: they exist if and
    /// only if its event does.
    pub scheduled_actions: &'a [ScheduledActionRecord],
    /// The arm id of this entity's scheduled action that the appended
    /// transition fired: its row is removed in the same commit, so a row
    /// never outlives its fire's event and a restart cannot fire it again.
    pub fired_scheduled_action: Option<&'a str>,
    /// The entity's query-plane row as the appended events leave it. A store
    /// whose [`EventStore::commits_entity_rows`] is true writes it in the
    /// journal's transaction, at the last appended sequence.
    pub projection: Option<ProjectionRow<'a>>,
    /// The entity's state-timer row as the appended events leave it, for a
    /// store whose [`EventStore::commits_entity_rows`] is true. None leaves
    /// the row as it is.
    pub state_timer: Option<StateTimerRow<'a>>,
}

/// An entity's query-plane row, written with the events that change it.
#[derive(Debug, Clone, Copy)]
pub enum ProjectionRow<'a> {
    /// The entity's status, its projected fields and its projected state.
    Upsert {
        status: &'a str,
        fields: &'a serde_json::Value,
        state: &'a serde_json::Value,
    },
    /// The entity was deleted: its row and its field-index rows go.
    Remove,
}

/// An entity's state-timer row, written with the events that change it.
#[derive(Debug, Clone, Copy)]
pub enum StateTimerRow<'a> {
    /// The events armed (or reset) a timer: this is its row.
    Arm(&'a StateTimerRecord),
    /// The entity is no longer where a timer waits: its row goes.
    End,
}

impl CommitRows<'_> {
    /// Whether the commit writes or removes a scheduled-action row.
    pub fn changes_scheduled_actions(&self) -> bool {
        !self.scheduled_actions.is_empty() || self.fired_scheduled_action.is_some()
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
    /// [`EventStore::append_with_rows`] with only key rows, so callers that
    /// only maintain keys are unchanged. The co-commit logic lives in
    /// `append_with_rows`, which backends override.
    fn append_with_keys(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[EntityKeyRow],
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        self.append_with_rows(
            persistence_id,
            expected_sequence,
            events,
            CommitRows {
                key_rows,
                ..CommitRows::default()
            },
        )
    }

    /// Append events and commit `rows` with them: declared key-index rows
    /// (ADR-0153), derived vector-index rows (ADR-0155) and the scheduled
    /// actions the transition armed. This is the single co-commit entry point
    /// the entity actor calls. Postgres and the sim store commit every kind in
    /// the journal's transaction; Turso commits the scheduled actions there and
    /// maintains the vector index write-behind (event first, index follows).
    /// When `reconcile_vectors` is true (the entity's type declares ≥1
    /// `[[vector]]` path) the store first DELETES all of the entity's vector
    /// rows, then inserts `vector_rows`. The sequence and atomicity contract is
    /// identical to `append`.
    ///
    /// The default keeps only the events: a store without a timer table cannot
    /// keep scheduled actions, so it logs that a restart loses them.
    fn append_with_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        if rows.changes_scheduled_actions() {
            tracing::error!(
                persistence_id,
                scheduled = rows.scheduled_actions.len(),
                "this event store keeps no scheduled actions; a restart loses them"
            );
        }
        self.append(persistence_id, expected_sequence, events)
    }

    /// Whether [`EventStore::append_with_rows`] writes
    /// [`CommitRows::projection`] and [`CommitRows::state_timer`] in the
    /// journal's transaction: the entity's query-plane row and timer row are
    /// then the journal's, and nothing writes them again after the commit.
    /// The default writes neither.
    fn commits_entity_rows(&self) -> bool {
        false
    }

    /// Every pending scheduled action of `tenant`: what a restart arms again.
    /// The default store keeps none.
    fn list_scheduled_actions(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ScheduledActionRecord>, PersistenceError>> + Send
    {
        let _ = tenant;
        async { Ok(Vec::new()) }
    }

    /// The pending scheduled actions of one record: what an arm compares
    /// with, so an action armed twice for the same time keeps one row. The
    /// default reads the tenant's rows and keeps the record's.
    fn list_entity_scheduled_actions(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ScheduledActionRecord>, PersistenceError>> + Send
    {
        let rows = self.list_scheduled_actions(tenant);
        async move {
            Ok(rows
                .await?
                .into_iter()
                .filter(|row| row.entity_type == entity_type && row.entity_id == entity_id)
                .collect())
        }
    }

    /// Durably cancel a scheduled action without deleting its row, by clearing
    /// its action. Idempotent, including when the row is already absent.
    /// Recovery must only delete cancelled rows, never dispatch them. Stores
    /// that keep rows override this; stores without rows can simply delete.
    fn cancel_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        self.delete_scheduled_action(tenant, entity_type, entity_id, arm_id)
    }

    /// Remove one scheduled action's row once it has fired. Idempotent.
    fn delete_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, entity_id, arm_id);
        async { Ok(()) }
    }

    /// Reconcile the derived vector-index rows for an **existing** entity to exactly
    /// `vector_rows` (ADR-0155), without appending a journal event: DELETE every
    /// existing row for `(tenant, entity_type, entity_id)`, then INSERT `vector_rows`.
    /// Idempotent, and an empty `vector_rows` PURGES the entity (used to clean up a
    /// deleted or un-embedded entity). Used by the backfill and by the Turso
    /// write-behind path. The default is a no-op (non-indexing backends); query-plane
    /// stores implement it.
    fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, entity_id, vector_rows);
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
    /// — every existing entity has had its declared vectors indexed by the backfill
    /// (ADR-0155 watermark, mirroring `mark_key_index_backfilled`). `vector_set` is
    /// the sorted, comma-joined declared vector-path NAMES the backfill covered, so a
    /// later declaration of an ADDITIONAL path is detected as a set change and the
    /// type is re-indexed. Idempotent. Default no-op.
    fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, vector_set);
        async { Ok(()) }
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

    /// The `entity_id`s that already have at least one `entity_vector_index` row for
    /// `(tenant, entity_type)`. Lets the vector backfill **resume** cheaply, skipping
    /// already-indexed entities. Default empty (no resumption). Mirrors
    /// `keyed_entity_ids_for_type`.
    fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        let _ = (tenant, entity_type);
        async { Ok(Vec::new()) }
    }

    /// Backfill declared key-index rows for an **existing** entity (ADR-0153),
    /// without appending a journal event. Idempotent: re-running yields the same
    /// rows. Used to populate `entity_key_index` for entities written before the
    /// declared key existed, so a keyed read can authoritatively prove absence
    /// (the per-tenant backfill watermark gates #324's retirement). The default
    /// is a no-op (non-indexing backends); query-plane stores upsert the rows.
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
    /// correctness bug. Such backends MUST keep the default no-op so they never
    /// become authoritative (their keyed misses fall back to the scan — correct,
    /// just not bounded). Postgres co-commits and overrides this; the sim store does
    /// too for DST. The default is a no-op.
    ///
    /// `key_set` is the sorted, comma-joined declared key NAMES the backfill just
    /// covered. It is recorded so a later declaration of an ADDITIONAL key is detected
    /// as a key-set change (the recorded set no longer equals the current one) and the
    /// type is re-keyed, instead of being wrongly treated as already complete.
    fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, key_set);
        async { Ok(()) }
    }

    /// The `(entity_type, key_set)` watermarks for `tenant` — each type whose
    /// `entity_key_index` backfill is complete, paired with the sorted comma-joined
    /// declared key names it covered. The read plane caches these so a keyed miss on a
    /// type resolves to authoritative absence ONLY when the covered key-set still equals
    /// the currently-declared one. Default empty (no backend authority → scan-safe).
    fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        let _ = tenant;
        async { Ok(Vec::new()) }
    }

    /// The `entity_id`s that already have at least one `entity_key_index` row for
    /// `(tenant, entity_type)`. Lets the backfill **resume** cheaply: it skips
    /// already-keyed entities (the expensive part is loading each entity's state),
    /// so a re-run after a partial pass only processes the remainder instead of
    /// re-loading all N. Default empty (no resumption — a backend without the index
    /// re-processes everything, which is correct, just not incremental).
    fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        let _ = (tenant, entity_type);
        async { Ok(Vec::new()) }
    }

    /// Atomically append events to multiple journals.
    ///
    /// Backends must either commit every append in `appends`, or commit none.
    /// This is the storage primitive composite actions need before they can
    /// persist cross-actor sub-writes as one physical unit.
    fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceAppendResult>, PersistenceError>> + Send;

    /// Read events from the journal, starting after the given sequence number.
    fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceEnvelope>, PersistenceError>> + Send;

    /// Read one page of the journal, newest first: the newest `limit` events
    /// whose sequence number is below `before_sequence` (the newest of the
    /// whole journal when it is `None`), returned in ascending sequence order.
    ///
    /// Backends read only those rows, so a page costs its own size and not the
    /// journal's. `limit` is a count of rows; a SQL backend binds it as a signed
    /// 64-bit `LIMIT`, so values above `i64::MAX` read the whole range.
    fn read_events_page(
        &self,
        persistence_id: &str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceEnvelope>, PersistenceError>> + Send;

    /// Creation time kept by the kernel, even after journal compaction. Stores
    /// with a catalog prefer its saved fact; otherwise read exactly sequence 1.
    fn first_event_at(
        &self,
        persistence_id: &str,
    ) -> impl std::future::Future<
        Output = Result<Option<chrono::DateTime<chrono::Utc>>, PersistenceError>,
    > + Send {
        async move {
            Ok(self
                .read_events_page(persistence_id, Some(2), 1)
                .await?
                .first()
                .filter(|event| event.sequence_nr == 1)
                .map(|event| {
                    event
                        .payload
                        .get("timestamp")
                        .and_then(|value| serde_json::from_value(value.clone()).ok())
                        .unwrap_or(event.metadata.timestamp)
                }))
        }
    }

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

    /// Last durable sequence, including the latest snapshot after compaction.
    /// SQL stores override this with indexed sequence-only reads.
    fn latest_sequence_nr(
        &self,
        persistence_id: &str,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        async move {
            let snapshot = self
                .load_snapshot(persistence_id)
                .await?
                .map_or(0, |(seq, _)| seq);
            let tail = self.read_events(persistence_id, snapshot).await?;
            Ok(tail
                .last()
                .map_or(snapshot, |event| event.sequence_nr.max(snapshot)))
        }
    }

    /// Read every journal head in one batch, without events or snapshot bodies.
    /// Used when the catalog lives outside the event store. Query-plane journals
    /// override this; unsupported journals fail boot repair rather than serving
    /// unchecked rows.
    fn journal_heads(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<JournalHead>, PersistenceError>> + Send {
        let _ = tenant;
        async {
            Err(PersistenceError::Storage(
                "batched journal heads unsupported".into(),
            ))
        }
    }

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
    /// The entity's query-plane row as the events leave it. A store whose
    /// [`EventStore::commits_entity_rows`] is true writes it in the batch's
    /// transaction, as [`CommitRows::projection`] in a single append.
    #[serde(default)]
    pub projection: Option<AppendProjection>,
    /// The last timer mutation in this append. None preserves the existing
    /// arm, including when a later event does not reset its clock.
    #[serde(default)]
    pub state_timer: Option<AppendStateTimer>,
}

/// An entity's query-plane row, owned, for an append in a batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AppendProjection {
    /// The entity's status, its projected fields and its projected state.
    Upsert {
        status: String,
        fields: serde_json::Value,
        state: serde_json::Value,
    },
    /// The entity was deleted: its row and its field-index rows go.
    Remove,
}

impl AppendProjection {
    /// The row as a single append's commit rows carry it.
    pub fn as_row(&self) -> ProjectionRow<'_> {
        match self {
            Self::Upsert {
                status,
                fields,
                state,
            } => ProjectionRow::Upsert {
                status,
                fields,
                state,
            },
            Self::Remove => ProjectionRow::Remove,
        }
    }
}

/// An owned timer mutation committed with a batch stream's events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AppendStateTimer {
    /// Entering or resetting a timed state replaces its durable arm.
    Arm(Box<StateTimerRecord>),
    /// Leaving a timed state or consuming a one-shot timer removes its arm.
    End,
}

impl AppendStateTimer {
    /// Borrow this mutation for the same writer a single append uses.
    pub fn as_row(&self) -> StateTimerRow<'_> {
        match self {
            Self::Arm(row) => StateTimerRow::Arm(row),
            Self::End => StateTimerRow::End,
        }
    }
}

impl PersistenceAppend {
    /// The rows this append commits with its events.
    pub fn commit_rows(&self) -> CommitRows<'_> {
        CommitRows {
            projection: self.projection.as_ref().map(AppendProjection::as_row),
            state_timer: self.state_timer.as_ref().map(AppendStateTimer::as_row),
            ..CommitRows::default()
        }
    }
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

    /// A write failed after its commit began and the journal could not be
    /// read to learn whether it landed: it may have.
    #[error("outcome unknown: {0}")]
    OutcomeUnknown(String),

    /// One event is larger than the server's limit for a single change
    /// (`TEMPER_MAX_EVENT_BYTES`). Nothing was written.
    #[error(
        "refused: `{event_type}` on {persistence_id} would record {bytes} bytes, over this server's limit of {limit} bytes for one change. Keep each record small: long lists as their own records (one record per item, pointing back to this one), and pictures or documents as files, holding only their ids here"
    )]
    EventTooLarge {
        event_type: String,
        persistence_id: String,
        bytes: usize,
        limit: usize,
    },

    /// The disk under the store has less room available than the server keeps
    /// free (`temper_server::storage::STORAGE_FLOOR_BYTES`). Nothing was
    /// written; writes go on by themselves once the disk has room again.
    #[error(
        "storage full: {} MB available on the disk of {dir}, under the {} MB this server keeps free; nothing was written, and writes go on once the disk has room",
        .available_bytes >> 20,
        .floor_bytes >> 20
    )]
    StorageFull {
        dir: String,
        available_bytes: u64,
        floor_bytes: u64,
    },
}

/// Convert backend-specific errors into [`PersistenceError::Storage`].
pub fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}
