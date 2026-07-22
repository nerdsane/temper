//! Journal, snapshot, and entity-enumeration methods on the EventStore facade.

macro_rules! event_store_journal_methods {
    () => {
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

    /// Check whether a content-bound atomic batch claim already committed.
    ///
    /// Returns `true` only for an exact intent match, `false` when the claim is
    /// absent, and an error when the same namespace/key names different work.
    /// Composite retry paths call this before current-state validation so an
    /// aged stream cannot hide a durable replay behind bounded actor history.
    fn batch_idempotency_committed(
        &self,
        claim: &PersistenceBatchIdempotency,
    ) -> impl std::future::Future<Output = Result<bool, PersistenceError>> + Send {
        let _ = claim;
        async { Ok(false) }
    }

    /// Read events from the journal, starting after the given sequence number.
    fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceEnvelope>, PersistenceError>> + Send;

    /// Read one bounded, ascending journal page after `from_sequence` and no later
    /// than the inclusive `through_sequence` captured by the caller.
    ///
    /// Implementations must apply `limit` at the storage boundary rather than
    /// fetching an unbounded suffix first. Callers must pass a positive limit.
    fn read_events_page(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        through_sequence: u64,
        limit: usize,
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

    /// Save a state snapshot without regressing the current generation.
    ///
    /// An older sequence and an identical same-sequence write are no-ops. A
    /// same-sequence write with different bytes replaces that source generation;
    /// a newer sequence advances it. The accept/no-op decision must cover every
    /// snapshot-derived mutation atomically.
    fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send;

    /// Save a state snapshot only while its exact derivation source is current.
    ///
    /// Implementations must validate `source` and apply the snapshot write in one
    /// stream-fenced transaction. The conservative default supports only
    /// [`SnapshotSourceFence::Unchecked`]; stores used by entity actors override
    /// this method so stale passivation and queued writers cannot replace a
    /// same-sequence snapshot generation.
    fn save_snapshot_if_source(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &SnapshotSourceFence,
        key_contract: Option<&str>,
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
        async move {
            let _ = key_contract;
            if !matches!(source, SnapshotSourceFence::Unchecked) {
                return Err(PersistenceError::SnapshotGenerationChanged);
            }
            self.save_snapshot(persistence_id, sequence_nr, snapshot)
                .await
        }
    }

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

    /// Capture the inclusive terminal entity ID for one bounded repair scan.
    /// Candidates created after this boundary are exact current-contract writes
    /// and must not extend an already-running traversal indefinitely.
    fn key_reconciliation_boundary(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, PersistenceError>> + Send {
        async move {
            Ok(self
                .list_entity_ids_for_key_reconciliation(tenant, entity_type)
                .await?
                .into_iter()
                .max())
        }
    }

    /// Read one deterministic, storage-bounded page of declared-key repair
    /// candidates after `after_entity_id`.
    ///
    /// Authoritative key stores must override this and derive `is_live` in the
    /// same storage query as the candidate page. The final reconciliation-
    /// revision CAS invalidates every page if a source changes during traversal.
    fn list_key_reconciliation_page(
        &self,
        tenant: &str,
        entity_type: &str,
        after_entity_id: Option<&str>,
        through_entity_id: &str,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<KeyReconciliationEntity>, PersistenceError>> + Send
    {
        async move {
            assert!(limit > 0, "key reconciliation page limit must be positive");
            if self.supports_authoritative_key_index() {
                return Err(PersistenceError::Storage(format!(
                    "authoritative key store did not implement bounded repair paging for {tenant}:{entity_type}"
                )));
            }
            let mut entity_ids = self
                .list_entity_ids_for_key_reconciliation(tenant, entity_type)
                .await?;
            entity_ids.sort();
            entity_ids.dedup();
            Ok(entity_ids
                .into_iter()
                .filter(|entity_id| {
                    after_entity_id.is_none_or(|cursor| entity_id.as_str() > cursor)
                        && entity_id.as_str() <= through_entity_id
                })
                .take(limit)
                .map(|entity_id| KeyReconciliationEntity {
                    entity_id,
                    is_live: true,
                })
                .collect())
        }
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

    };
}

pub(crate) use event_store_journal_methods;
