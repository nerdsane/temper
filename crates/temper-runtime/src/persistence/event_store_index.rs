//! Declared-key and vector-index methods on the EventStore facade.

macro_rules! event_store_index_methods {
    () => {
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
                snapshot_source: SnapshotSourceFence::Unchecked,
            },
        )
    }

    /// Append events and co-commit BOTH declared key-index rows (ADR-0153) and
    /// derived vector-index rows (ADR-0155) in the **same transaction** as the
    /// journal append. This is the single co-commit entry point the entity actor
    /// calls. The default ignores the index kinds and delegates to
    /// [`EventStore::append`] only when the snapshot source is unchecked. A
    /// snapshot-fenced append fails closed unless the backend overrides this
    /// method and validates the source atomically with the journal write. Stores
    /// with a query plane that co-commit (postgres, sim) override it; Turso also
    /// overrides it to maintain the vector index write-behind (event first, index
    /// follows). When `reconciliation.keys` is true
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
        let _ = (key_rows, vector_rows);
        async move {
            if !matches!(
                reconciliation.snapshot_source,
                SnapshotSourceFence::Unchecked
            ) {
                return Err(PersistenceError::SnapshotGenerationChanged);
            }
            self.append(persistence_id, expected_sequence, events).await
        }
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
    /// ADR-0192), without appending a journal event. The store first validates that
    /// `contract_fence` still identifies the tenant/type contract, exact journal
    /// boundary, exact snapshot generation, and entity liveness under which replay
    /// derived `key_rows`, then
    /// validates `expected_sequence`. Only while all fences hold does it DELETE every existing
    /// row for `(tenant, entity_type, entity_id)` and INSERT `key_rows`. Idempotent,
    /// and an empty set purges stale ownership for deleted or currently unkeyable
    /// entities. A concurrent type-contract change fails with
    /// [`PersistenceError::KeyContractChanged`]; a concurrent journal-source change
    /// fails with [`PersistenceError::JournalBoundaryChanged`]; a concurrent liveness
    /// change fails with [`PersistenceError::EntityLivenessChanged`]; a concurrent
    /// snapshot change fails with [`PersistenceError::SnapshotGenerationChanged`]; a concurrent
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

    /// Every durable `(tenant, entity_type)` with an activated key contract.
    ///
    /// Startup uses this authority to retire contracts whose replace-mode spec
    /// deletion committed before the later runtime activation transaction.
    fn key_index_activated_contracts(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        let authoritative = self.supports_authoritative_key_index();
        async move {
            if authoritative {
                return Err(PersistenceError::Storage(
                    "authoritative key store did not implement activated-contract enumeration"
                        .to_string(),
                ));
            }
            Ok(Vec::new())
        }
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

    /// Establish a key contract before its spec becomes live. When
    /// `purge_existing_rows` is true, the contract change and type-wide removal
    /// of prior ownership rows must be atomic under the same contract fence.
    fn activate_key_index_contract(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        purge_existing_rows: bool,
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
        async move {
            if self.supports_authoritative_key_index() {
                return Err(PersistenceError::Storage(format!(
                    "authoritative key store did not implement fenced contract activation for {tenant}:{entity_type}"
                )));
            }
            let revision = self
                .begin_key_index_backfill(tenant, entity_type, key_set)
                .await?;
            let _ = purge_existing_rows;
            Ok(revision)
        }
    }

    /// Atomically activate every changed entity type in one spec publication
    /// and return the monotonic epoch assigned to each type. Authoritative
    /// backends must override multi-type activation so a failure cannot leave a
    /// partially fenced tenant while the old registry remains live.
    fn activate_key_index_contracts(
        &self,
        tenant: &str,
        activations: &[KeyContractActivation],
    ) -> impl std::future::Future<
        Output = Result<std::collections::BTreeMap<String, u64>, PersistenceError>,
    > + Send {
        async move {
            if self.supports_authoritative_key_index() {
                return Err(PersistenceError::Storage(format!(
                    "authoritative key store did not implement atomic spec-fingerprinted activation for {tenant}"
                )));
            }
            let mut epochs = std::collections::BTreeMap::new();
            for activation in activations {
                let epoch = self
                    .activate_key_index_contract(
                        tenant,
                        &activation.entity_type,
                        &activation.key_set,
                        activation.purge_existing_rows,
                    )
                    .await?;
                epochs.insert(activation.entity_type.clone(), epoch);
            }
            Ok(epochs)
        }
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
    /// exact ADR-0192 repair does not use presence as proof of completeness because
    /// an existing row may itself be stale. Default empty.
    fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
        let _ = (tenant, entity_type);
        async { Ok(Vec::new()) }
    }

    };
}

pub(crate) use event_store_index_methods;
