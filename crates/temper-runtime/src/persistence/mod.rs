use serde::{Deserialize, Serialize};

mod batch;
mod index;
mod types;
pub use batch::{PersistenceAppend, PersistenceAppendResult};
pub use index::{
    EntityKeyLookup, EntityKeyRow, EntityVectorCandidate, EntityVectorRow, IndexReconciliation,
    KeyIndexBackfillFence, pack_f32_le, unpack_f32_le,
};
pub use types::{JournalBoundary, PersistenceEnvelope, PersistenceError, storage_error};

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

/// Trait for the event store backend (implemented by temper-store-postgres).
/// Uses desugared async-in-trait to enforce Send bounds on futures.
pub trait EventStore: Send + Sync + 'static {
    /// Whether this backend maintains declared-key rows exactly on every live write,
    /// can repair them from replay, and persists coverage watermarks. Only such a
    /// backend may answer keyed hits/misses as an authoritative ownership oracle.
    /// Defaults to false so no-op index methods can never create false authority.
    fn supports_authoritative_key_index(&self) -> bool {
        false
    }

    /// Append events to the journal.
    fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send;

    /// Append events and co-commit declared key-index rows (ADR-0153) in the
    /// **same transaction** as the journal append. `key_rows` is the entity's exact
    /// current key set, including an empty set after delete or key removal. A thin
    /// forwarder to [`EventStore::append_with_index_rows`] with key reconciliation
    /// enabled and no vector rows. The co-commit logic lives in
    /// `append_with_index_rows`, which query-plane backends override.
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
                key_set_signature: None,
                vectors: false,
            },
        )
    }

    /// Append events and co-commit BOTH declared key-index rows (ADR-0153) and
    /// derived vector-index rows (ADR-0155) in the **same transaction** as the
    /// journal append. This is the single co-commit entry point the entity actor
    /// calls. The default ignores the index kinds and delegates to
    /// [`EventStore::append`] — stores with a query plane that co-commit (postgres,
    /// sim) override it; Turso also overrides it to maintain the vector index
    /// write-behind (event first, index follows). When `reconciliation.keys` is true
    /// (the entity's type declares ≥1 `[[key]]`) the store validates every current
    /// claim, then DELETES all prior key rows for the entity and inserts `key_rows` in
    /// the journal transaction. Empty rows therefore release ownership. Likewise,
    /// when `reconciliation.vectors` is true (the entity's type declares ≥1
    /// `[[vector]]` path) the store first DELETES all of the entity's vector rows,
    /// then inserts `vector_rows`. The sequence and atomicity contract is identical
    /// to `append`.
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

    /// Reconcile declared key-index rows for an **existing** entity (ADR-0153,
    /// ADR-0171), without appending a journal event. The store first validates that
    /// `contract_fence` still identifies the tenant/type contract, exact journal
    /// boundary, and entity liveness under which replay derived `key_rows`, then
    /// validates `expected_sequence`. Only while all fences hold does it DELETE every existing
    /// row for `(tenant, entity_type, entity_id)` and INSERT `key_rows`. Idempotent,
    /// and an empty set purges stale ownership for deleted or currently unkeyable
    /// entities. A concurrent type-contract change fails with
    /// [`PersistenceError::KeyContractChanged`]; a concurrent journal-source change
    /// fails with [`PersistenceError::JournalBoundaryChanged`]; a concurrent liveness
    /// change fails with [`PersistenceError::EntityLivenessChanged`]; a concurrent
    /// durable sequence advance fails with [`PersistenceError::ConcurrencyViolation`]. Used to populate and repair
    /// `entity_key_index` before a keyed read can treat absence as authoritative. The
    /// default is a no-op (non-indexing backends).
    fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        contract_fence: KeyIndexBackfillFence<'_>,
        key_rows: &[EntityKeyRow],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (
            tenant,
            entity_type,
            entity_id,
            expected_sequence,
            contract_fence,
            key_rows,
        );
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

    /// Resolve a declared-key owner together with its co-committed journal
    /// generation. Authoritative backends override this from the same row used
    /// by [`EventStore::lookup_by_key`]. The compatibility default preserves
    /// non-authoritative/custom stores while assigning generation zero.
    fn lookup_by_key_with_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> impl std::future::Future<Output = Result<Option<EntityKeyLookup>, PersistenceError>> + Send
    {
        async move {
            self.lookup_by_key(tenant, entity_type, key_name, key_hash)
                .await
                .map(|owner| {
                    owner.map(|entity_id| EntityKeyLookup {
                        entity_id,
                        sequence_nr: 0,
                    })
                })
        }
    }

    /// Record that `entity_key_index` is **complete** for `(tenant, entity_type)`
    /// — every existing entity of that type has been keyed by the backfill
    /// (ADR-0153/0171 watermark). Only once set may a keyed read trust an indexed
    /// hit or miss. A miss is then authoritative absence, retiring the full-type
    /// reconcile scan (#324) for that type. Idempotent.
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
    /// `key_set` is the derivation-contract version plus every sorted declaration's
    /// name and ordered properties. It is recorded so either a definition change or
    /// a reconciliation-semantics upgrade forces a complete repair pass.
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
    /// `entity_key_index` backfill is complete, paired with the versioned declared-key
    /// signature it covered. The read plane caches these so a keyed hit or miss is
    /// authoritative ONLY when the covered signature still equals the current one.
    /// Default empty (no backend authority → scan-safe).
    fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        let _ = tenant;
        async { Ok(Vec::new()) }
    }

    /// Monotonic revision of the live declared-key reconciliation universe for one
    /// type. It changes when a durable write uses a different key signature or when
    /// a snapshot/catalog-only mutation is not already represented by the journal.
    /// Backfill captures this before enumeration and conditionally publishes its
    /// watermark against it, so neither a contract race nor a newly durable entity
    /// can certify stale coverage.
    fn key_index_reconciliation_revision(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        let _ = (tenant, entity_type);
        async { Ok(0) }
    }

    /// Establish `key_set` as the target contract before a full backfill starts and
    /// return its monotonic revision. This invalidates older coverage up front. A
    /// concurrent live write under a different signature must then advance the
    /// revision, causing the final conditional watermark publication to fail.
    fn begin_key_index_backfill(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        let _ = (tenant, entity_type, key_set);
        async { Ok(0) }
    }

    /// Publish a coverage watermark only if the type's live key-contract revision is
    /// still `expected_revision` AND its signature is still `key_set`. Returns false
    /// when a concurrent write changed either; callers must leave the type scan-safe
    /// and retry a full repair.
    fn mark_key_index_backfilled_if_revision(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        expected_revision: u64,
    ) -> impl std::future::Future<Output = Result<bool, PersistenceError>> + Send {
        let _ = expected_revision;
        async move {
            self.mark_key_index_backfilled(tenant, entity_type, key_set)
                .await?;
            Ok(true)
        }
    }

    /// The `entity_id`s that already have at least one `entity_key_index` row for
    /// `(tenant, entity_type)`. Retained for index inspection and backend conformance;
    /// exact ADR-0171 repair does not use presence as proof of completeness because
    /// an existing row may itself be stale. Default empty.
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
    /// Backends must either commit every append in `appends`, or commit none. For
    /// each item whose `reconcile_keys` is true, its `key_rows` are the exact final
    /// declared-key set and must change atomically with every journal in the batch.
    /// This is the storage primitive composite actions use for one physical unit.
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

    /// Return the exact durable high-water and terminal lifecycle boundary for one
    /// stream.
    ///
    /// Snapshots are derived acceleration state and cannot outrank a terminal
    /// journal event. Recovery also checks the high-water after replay so a
    /// fault-truncated prefix cannot be accepted as current state. Backends whose
    /// normal reads may be truncated must override this compatibility scan with an
    /// exact metadata lookup.
    fn journal_boundary(
        &self,
        persistence_id: &str,
    ) -> impl std::future::Future<Output = Result<JournalBoundary, PersistenceError>> + Send {
        async move {
            let events = self.read_events(persistence_id, 0).await?;
            Ok(JournalBoundary {
                latest_sequence: events.last().map(|event| event.sequence_nr).unwrap_or(0),
                first_terminal_sequence: events
                    .iter()
                    .find(|event| event.transitions_to_deleted())
                    .map(|event| event.sequence_nr),
            })
        }
    }

    /// Return the first durable terminal sequence, if any.
    ///
    /// This compatibility helper delegates to [`EventStore::journal_boundary`];
    /// recovery should use the complete boundary so it can also prove replay
    /// completeness.
    fn terminal_tombstone_sequence(
        &self,
        persistence_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<u64>, PersistenceError>> + Send {
        async move {
            Ok(self
                .journal_boundary(persistence_id)
                .await?
                .first_terminal_sequence)
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

    /// List every stream or derived-key owner that must participate in exact key
    /// reconciliation for one `(tenant, entity_type)`. Unlike normal live-entity
    /// enumeration, this includes deleted journal streams and key-index-only
    /// phantoms so repair can purge their stale ownership before watermarking.
    /// Backends without a separate authoritative key index inherit the ordinary
    /// type enumeration.
    fn list_entity_ids_for_key_reconciliation(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        self.list_entity_ids_by_type(tenant, entity_type)
    }

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
