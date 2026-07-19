use serde::{Deserialize, Serialize};

mod indexing;
pub use indexing::{
    EntityKeyRow, EntityVectorCandidate, EntityVectorRow, PersistenceAppend,
    PersistenceAppendResult, pack_f32_le, unpack_f32_le,
};
mod types;
pub use types::{
    CompositeEvent, CompositeEventSubWrite, EventMetadata, PersistenceEnvelope, PersistenceError,
    storage_error,
};

/// Event type used for the parent-journal record of a Composite action.
///
/// Concrete sub-write events remain the state-changing events on their target
/// journals. This event records the composite intent and the exact sub-write
/// journals/idempotency keys that were committed atomically with it.
pub const COMPOSITE_EVENT_TYPE: &str = "CompositeEvent";

/// Marker trait for domain events.
/// Events must be serializable (for persistence) and Send + 'static (for async).
pub trait DomainEvent:
    Send + Serialize + for<'de> Deserialize<'de> + std::fmt::Debug + 'static
{
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
            false,
            None,
        )
    }

    /// Append events and co-commit BOTH declared key-index rows (ADR-0153) and
    /// derived vector-index rows (ADR-0155) in the **same transaction** as the
    /// journal append. This is the single co-commit entry point the entity actor
    /// calls. The default ignores the index kinds and delegates to
    /// [`EventStore::append`] — stores with a query plane that co-commit (postgres,
    /// sim, Turso) override it. When `reconcile_vectors` is true
    /// (the entity's type declares ≥1 `[[vector]]` path) the store first DELETES all
    /// of the entity's vector rows, then inserts `vector_rows` — so a delete
    /// transition or a cleared vector/model property purges the stale rows instead of
    /// leaving them to be ranked forever. The sequence and atomicity contract is
    /// identical to `append`. `spec_declaration_fingerprint` binds the writer's
    /// compiled table to durable spec authority; indexing stores reject a stale
    /// fingerprint before advancing the journal. Callers that reconcile vectors
    /// must always provide it.
    #[expect(
        clippy::too_many_arguments,
        reason = "journal, key, vector, and declaration data form one atomic storage boundary"
    )]
    fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
        spec_declaration_fingerprint: Option<&str>,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        let _ = (
            key_rows,
            vector_rows,
            reconcile_vectors,
            spec_declaration_fingerprint,
        );
        self.append(persistence_id, expected_sequence, events)
    }

    /// Persist one spec declaration fingerprint or absence tombstone.
    ///
    /// SQL stores derive this authority from their transactional spec catalog.
    /// Deterministic stores override this hook so the production hot-load path
    /// drives the same authority before publishing a rebuilt registry.
    fn persist_spec_declaration(
        &self,
        tenant: &str,
        entity_type: &str,
        declaration_fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        let _ = (tenant, entity_type, declaration_fingerprint);
        async { Ok(0) }
    }

    /// Return entity types whose durable declarations are currently present.
    ///
    /// The default is empty because SQL-backed servers enumerate their catalog
    /// through the metadata store. Deterministic stores override this for
    /// replacement retry/restart parity.
    fn spec_declaration_entity_types(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        let _ = tenant;
        async { Ok(Vec::new()) }
    }

    /// Begin reconciliation and return its durable generation (ADR-0181).
    /// `declaration_revision` is monotonic; `declaration_fingerprint` identifies the
    /// IOA source. Durable backends resolve both against a tombstone-preserving
    /// declaration authority, so a stale caller cannot win by arriving last or after
    /// delete/re-add. The returned token fences every replacement and watermark.
    /// Non-indexing backends reject the operation.
    fn begin_vector_index_reconciliation(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _vector_set: &str,
        _declaration_revision: u64,
        _declaration_fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        async {
            Err(PersistenceError::Storage(
                "vector-index reconciliation is unsupported by this event store".to_string(),
            ))
        }
    }

    /// Reconcile the derived vector-index rows for an **existing** entity to exactly
    /// `vector_rows` (ADR-0181), without appending a journal event.
    /// `reconciliation_generation` identifies the declaration set and
    /// `observed_sequence` is the journal position from which the rows were rebuilt.
    /// Stores reject a generation that is no longer current, and within the current
    /// generation atomically replace rows only when the sequence is at least the
    /// entity's retained vector-index version. A lower sequence is a successful
    /// no-op. The version survives an empty row set, so stale work cannot resurrect a
    /// deleted/unembedded entity. Equal-sequence replay is idempotent.
    fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        reconciliation_generation: u64,
        observed_sequence: u64,
        vector_rows: &[EntityVectorRow],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (
            tenant,
            entity_type,
            entity_id,
            reconciliation_generation,
            observed_sequence,
            vector_rows,
        );
        async {
            Err(PersistenceError::Storage(
                "vector-index reconciliation is unsupported by this event store".to_string(),
            ))
        }
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
    /// the revisioned signature of every covered vector declaration (name, property,
    /// model property, dimensions, and metric), so any declaration change re-indexes
    /// the type. The durable `reconciliation_generation` must still be current;
    /// otherwise the stale completion claim is rejected. Idempotent within one
    /// generation. Non-indexing backends reject the operation explicitly.
    fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        reconciliation_generation: u64,
        vector_set: &str,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        let _ = (tenant, entity_type, reconciliation_generation, vector_set);
        async {
            Err(PersistenceError::Storage(
                "vector-index reconciliation is unsupported by this event store".to_string(),
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

    /// Entity types with durable vector-reconciliation state for `tenant`
    /// (ADR-0181): a generation row, retained per-entity fence, or candidate row.
    /// Unlike completion watermarks, this state survives an interrupted
    /// reconciliation and includes generation-zero live/legacy rows. The coordinator
    /// uses it as a work source so remove-all declarations cannot strand candidates.
    /// Default empty for non-indexing backends.
    fn vector_reconciliation_entity_types(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
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

    /// List every durable journal stream that a vector-index repair must reconcile
    /// for `(tenant, entity_type)`, including deleted streams (ADR-0181). Active
    /// entity listing deliberately excludes deletions on some backends, but repair
    /// must retain a sequence tombstone for them so stale rows cannot survive or be
    /// resurrected. Backends whose normal listing already includes the complete
    /// journal set may use this default.
    fn list_vector_repair_entity_ids(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        self.list_entity_ids_by_type(tenant, entity_type)
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
