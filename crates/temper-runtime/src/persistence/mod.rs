use serde::{Deserialize, Serialize};

pub mod schema_deployment;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        )
    }

    /// Append events and co-commit BOTH declared key-index rows (ADR-0153) and
    /// derived vector-index rows (ADR-0155) in the **same transaction** as the
    /// journal append. This is the single co-commit entry point the entity actor
    /// calls. The default ignores the index kinds and delegates to
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
        reconcile_vectors: bool,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        let _ = (key_rows, vector_rows, reconcile_vectors);
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

    /// Read at most `limit` events after `from_sequence`, in sequence order.
    fn read_events_limited(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceEnvelope>, PersistenceError>> + Send
    {
        async move {
            let mut events = self.read_events(persistence_id, from_sequence).await?;
            events.truncate(limit);
            Ok(events)
        }
    }

    /// Read at most the newest `limit` events, returned in ascending sequence order.
    fn read_latest_events(
        &self,
        persistence_id: &str,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceEnvelope>, PersistenceError>> + Send
    {
        async move {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let mut events = self.read_events(persistence_id, 0).await?;
            if events.len() > limit {
                events.drain(..events.len() - limit);
            }
            Ok(events)
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

    /// Page every durable journal identity, including deleted entities.
    ///
    /// `after` is an exclusive `(entity_type, entity_id)` cursor. Unlike the
    /// query-plane entity listings, this storage-maintenance API must retain
    /// tombstoned journals so durable side work cannot become undiscoverable.
    fn list_journal_ids_page(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        async move {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let mut entities = self.list_entity_ids(tenant).await?;
            if let Some(entity_type) = entity_type {
                entities.retain(|(found_type, _)| found_type == entity_type);
            }
            entities.sort();
            if let Some((after_type, after_id)) = after {
                entities.retain(|(entity_type, entity_id)| {
                    (entity_type.as_str(), entity_id.as_str()) > (after_type, after_id)
                });
            }
            entities.truncate(limit);
            Ok(entities)
        }
    }

    /// Page durable entity IDs for one immutable scoped-schema journal set.
    ///
    /// Scoped actors encode the bundle digest in the durable journal identity.
    /// Implementations must apply `limit` before returning so migration and
    /// query callers never need an unbounded journal scan.
    fn list_scoped_entity_ids_page(
        &self,
        tenant: &str,
        entity_type: &str,
        bundle_digest: &str,
        after_entity_id: Option<&str>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        async move {
            if limit == 0 {
                return Ok(Vec::new());
            }
            const JOURNAL_PAGE_BUDGET: usize = 256;
            let suffix = format!(":schema:{bundle_digest}");
            let mut cursor: Option<(String, String)> = None;
            let mut entity_ids = Vec::new();
            while entity_ids.len() < limit {
                let journals = self
                    .list_journal_ids_page(
                        tenant,
                        Some(entity_type),
                        cursor
                            .as_ref()
                            .map(|(found_type, id)| (found_type.as_str(), id.as_str())),
                        JOURNAL_PAGE_BUDGET,
                    )
                    .await?;
                let page_len = journals.len();
                let Some(last) = journals.last().cloned() else {
                    break;
                };
                cursor = Some(last);
                entity_ids.extend(journals.into_iter().filter_map(|(_, journal_entity_id)| {
                    journal_entity_id
                        .strip_suffix(&suffix)
                        .filter(|entity_id| after_entity_id.is_none_or(|after| *entity_id > after))
                        .map(str::to_string)
                }));
                if page_len < JOURNAL_PAGE_BUDGET {
                    break;
                }
            }
            entity_ids.sort();
            entity_ids.truncate(limit);
            Ok(entity_ids)
        }
    }

    /// Return the immutable bundle digests that have a durable journal for one
    /// scoped entity identity.
    ///
    /// Implementations must apply `limit` in the backing store. Callers use a
    /// budget of three so a valid two-sided cutover can be distinguished from
    /// an invalid third durable identity.
    fn scoped_entity_bundle_digests(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        async {
            Err(PersistenceError::Storage(
                "scoped entity pin lookup is unsupported by this event store".to_string(),
            ))
        }
    }

    /// Return the monotonic number of committed events for one bundle digest.
    ///
    /// Migration uses this as a bounded catch-up fence: a complete keyset pass
    /// is stable only when the value is unchanged from pass start to pass end.
    fn scoped_bundle_write_version(
        &self,
        tenant: &str,
        bundle_digest: &str,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        async move {
            const JOURNAL_PAGE_BUDGET: usize = 256;
            let suffix = format!(":schema:{bundle_digest}");
            let mut cursor: Option<(String, String)> = None;
            let mut version = 0_u64;
            loop {
                let journals = self
                    .list_journal_ids_page(
                        tenant,
                        None,
                        cursor
                            .as_ref()
                            .map(|(entity_type, id)| (entity_type.as_str(), id.as_str())),
                        JOURNAL_PAGE_BUDGET,
                    )
                    .await?;
                let page_len = journals.len();
                let Some(last) = journals.last().cloned() else {
                    break;
                };
                cursor = Some(last);
                for (entity_type, journal_entity_id) in journals {
                    if journal_entity_id.ends_with(&suffix) {
                        let persistence_id = format!("{tenant}:{entity_type}:{journal_entity_id}");
                        let count = self.read_events(&persistence_id, 0).await?.len();
                        version = version
                            .checked_add(u64::try_from(count).map_err(|_| {
                                PersistenceError::Storage("schema write version exhausted".into())
                            })?)
                            .ok_or_else(|| {
                                PersistenceError::Storage("schema write version exhausted".into())
                            })?;
                    }
                }
                if page_len < JOURNAL_PAGE_BUDGET {
                    break;
                }
            }
            Ok(version)
        }
    }
}

/// A persisted event with metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
