mod query_projection;
pub mod schema_deployment;
mod stream_descriptor;
mod types;
#[macro_use]
mod unscoped_methods;
#[macro_use]
mod scoped_methods;
pub use query_projection::{QueryProjectionOrder, QueryProjectionOrderTarget};
pub use stream_descriptor::*;
pub use types::*;

/// Event-store backend contract.
pub trait EventStore: Send + Sync + 'static {
    fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send;

    /// Append events and co-commit declared key-index rows (ADR-0153).
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

    impl_unscoped_event_store_methods!();

    impl_scoped_event_store_methods!();

    /// Return the monotonic number of committed events for one bundle digest.
    ///
    /// Migration uses this as a bounded catch-up fence: a complete keyset pass
    /// is stable only when the value is unchanged from pass start to pass end.
    fn scoped_bundle_write_version(
        &self,
        tenant: &str,
        scope: &schema_deployment::SchemaScope,
        bundle_digest: &str,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        async move {
            const JOURNAL_PAGE_BUDGET: usize = 256;
            let suffix = schema_deployment::scoped_journal_pin_suffix(
                &schema_deployment::SchemaExecutionPin {
                    scope: scope.clone(),
                    bundle_digest: bundle_digest.to_string(),
                },
            );
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
