//! Hydrate indexes from the durable store.

use crate::runtime_metrics;
use crate::state::{QueryProjectionReplayParityReport, ServerState, projection_backfill};
use temper_runtime::tenant::TenantId;
use tracing::instrument;

impl ServerState {
    /// Mark every entity type observed in `entities` as fully hydrated from the
    /// durable store, so `list_entity_ids_lazy` serves it from the in-memory
    /// index without a redundant store scan.
    fn mark_types_hydrated(&self, tenant: &TenantId, entities: &[(String, String)]) {
        let keys: std::collections::BTreeSet<String> = entities
            .iter()
            .map(|(entity_type, _entity_id)| format!("{tenant}:{entity_type}"))
            .collect();
        if keys.is_empty() {
            return;
        }
        self.entity_index_hydrated
            .write()
            .expect("entity index hydrated lock poisoned")
            .extend(keys);
    }

    /// Populate `entity_index` from the event store without spawning actors.
    ///
    /// This is the memory-safe startup/list path: we discover persisted
    /// entities while deferring actor allocation until first access.
    #[instrument(skip_all, fields(otel.name = "entity.populate_index_from_store", tenant = %tenant))]
    pub async fn populate_index_from_store(&self, tenant: &TenantId) {
        let Some((store, _backend)) = self.event_journal() else {
            return;
        };

        match store.list_entity_ids(tenant.as_str()).await {
            Ok(entities) => {
                {
                    let mut index = self
                        .entity_index
                        .write()
                        .expect("entity index lock poisoned");
                    for (entity_type, entity_id) in &entities {
                        let index_key = format!("{tenant}:{entity_type}");
                        index
                            .entry(index_key)
                            .or_default()
                            .insert(entity_id.clone());
                    }
                } // write lock dropped before metrics call
                // A full-store scan is authoritative for every type it observed.
                self.mark_types_hydrated(tenant, &entities);
                tracing::info!(
                    tenant = %tenant,
                    count = entities.len(),
                    "populated entity index from event store"
                );
                runtime_metrics::record_server_state_metrics(self);
            }
            Err(e) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %e,
                    "failed to populate entity index from event store"
                );
            }
        }
    }

    /// Populate `entity_index` for one entity type from the event store.
    #[instrument(skip_all, fields(
        otel.name = "entity.populate_index_from_store_by_type",
        tenant = %tenant,
        entity_type,
    ))]
    pub async fn populate_index_from_store_by_type(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> usize {
        let Some((store, _backend)) = self.event_journal() else {
            return 0;
        };

        match store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(entity_ids) => {
                let count = entity_ids.len();
                {
                    let mut index = self
                        .entity_index
                        .write()
                        .expect("entity index lock poisoned");
                    let index_key = format!("{tenant}:{entity_type}");
                    let ids = index.entry(index_key).or_default();
                    for entity_id in entity_ids {
                        ids.insert(entity_id);
                    }
                }
                // The store scan is authoritative for this type: mark it fully
                // hydrated so `list_entity_ids_lazy` can serve from the index.
                self.entity_index_hydrated
                    .write()
                    .expect("entity index hydrated lock poisoned")
                    .insert(format!("{tenant}:{entity_type}"));
                tracing::info!(
                    tenant = %tenant,
                    entity_type,
                    count,
                    "populated typed entity index from event store"
                );
                runtime_metrics::record_server_state_metrics(self);
                count
            }
            Err(e) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type,
                    error = %e,
                    "failed to populate typed entity index from event store"
                );
                0
            }
        }
    }

    /// Populate the durable query-plane projections for collection reads.
    ///
    /// Two-phase approach:
    /// 1. **Snapshot pass** — cheap: deserialises snapshots for entities that have them.
    /// 2. **Persistence replay pass** — reconstructs state directly from the event log.
    ///
    /// Runs once as a background task after startup.  New entities created after
    /// boot are indexed via `run_post_dispatch_effects` step 8.
    #[instrument(skip_all, fields(otel.name = "entity.populate_field_index", tenant = %tenant))]
    pub async fn populate_field_index_from_snapshots(&self, tenant: &TenantId) {
        projection_backfill::populate_field_index_from_snapshots(self, tenant).await;
    }

    /// Backfill `entity_key_index` for declared-key entity types (ADR-0153), so a
    /// keyed read can authoritatively prove absence for pre-existing entities.
    ///
    /// Independent of [`Self::populate_field_index_from_snapshots`]: the declared
    /// key is `K` (1–3) tiny rows per entity, far cheaper than the broad `S`-wide
    /// field-index re-scan, so it is gated and scheduled on its own rather than
    /// riding the expensive projection backfill. Runs once as a background task;
    /// entities written after boot are keyed inline at write time.
    #[instrument(skip_all, fields(otel.name = "entity.populate_key_index", tenant = %tenant))]
    pub async fn populate_key_index_from_snapshots(&self, tenant: &TenantId) {
        projection_backfill::populate_key_index_from_snapshots(self, tenant).await;
    }

    /// ADR-0155: backfill `entity_vector_index` for pre-existing entities of every
    /// vector-declaring type and record the watermark. Idempotent; entities written
    /// after boot maintain their vectors inline (co-commit) or write-behind.
    #[instrument(skip_all, fields(otel.name = "entity.populate_vector_index", tenant = %tenant))]
    pub async fn populate_vector_index_from_snapshots(&self, tenant: &TenantId) {
        projection_backfill::populate_vector_index_from_snapshots(self, tenant).await;
    }

    /// Hydrate the per-tenant `entity_key_index` watermark cache once from the durable
    /// watermark (ADR-0153). Safe to call repeatedly; conservative on any failure (leaves
    /// the type uncovered → a keyed miss falls back to the scan, never a wrong "absent").
    async fn ensure_key_index_watermarks_loaded(&self, tenant: &TenantId) {
        let already_loaded = self
            .key_index_watermarks_loaded
            .read()
            .expect("key index watermarks-loaded lock poisoned")
            .contains(tenant.as_str());
        if already_loaded {
            return;
        }
        if let Some((store, _)) = self.event_journal() {
            if let Ok(types) = store.key_index_backfilled_types(tenant.as_str()).await {
                let mut cache = self
                    .key_index_backfilled
                    .write()
                    .expect("key index backfilled lock poisoned");
                for (et, key_set) in types {
                    cache.insert(format!("{tenant}:{et}"), key_set);
                }
            }
            // Mark loaded even if the query errored: an error means "no authority
            // yet", which is the safe scan-fallback state, and a completing
            // backfill sets the cache entry directly via `mark_key_index_backfilled`.
            self.key_index_watermarks_loaded
                .write()
                .expect("key index watermarks-loaded lock poisoned")
                .insert(tenant.to_string());
        }
    }

    /// The declared key-set the `entity_key_index` backfill covered for `(tenant,
    /// entity_type)`, or `None` if the type was never backfilled. The value is the
    /// sorted comma-joined declared key names (see [`declared_key_set_signature`]).
    pub(crate) async fn key_index_backfill_covered_key_set(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<String> {
        self.ensure_key_index_watermarks_loaded(tenant).await;
        self.key_index_backfilled
            .read()
            .expect("key index backfilled lock poisoned")
            .get(&format!("{tenant}:{entity_type}"))
            .cloned()
    }

    /// Whether `entity_key_index` is complete for `(tenant, entity_type)` under the
    /// CURRENT declared key-set — the ADR-0153 backfill watermark, made key-set aware
    /// (ARN-68). True only when the covered key-set equals `current_key_set`, so a
    /// keyed read MISS is authoritative absence only once EVERY currently-declared key
    /// is backfilled; a newly-declared key reads as incomplete (scan-safe) until it is
    /// re-keyed. Conservative on any failure (returns false → keyed miss falls back to
    /// the scan, never a wrong "absent").
    pub(crate) async fn key_index_backfill_complete(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        current_key_set: &str,
    ) -> bool {
        self.key_index_backfill_covered_key_set(tenant, entity_type)
            .await
            .as_deref()
            == Some(current_key_set)
    }

    /// Record (durably + in the read-path cache) that `entity_key_index` is complete
    /// for `(tenant, entity_type)` covering exactly `key_set` (the sorted comma-joined
    /// declared key names). Called by the backfill once it has keyed every existing
    /// entity of the type, so subsequent keyed misses resolve to absence without
    /// scanning (ADR-0153). Overwrites any stale key-set from an earlier declaration.
    pub(crate) async fn mark_key_index_backfilled(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        key_set: &str,
    ) {
        if let Some((store, _)) = self.event_journal()
            && let Err(e) = store
                .mark_key_index_backfilled(tenant.as_str(), entity_type, key_set)
                .await
        {
            tracing::error!(
                tenant = %tenant, entity_type, error = %e,
                "failed to persist key-index backfill watermark"
            );
            return;
        }
        self.key_index_backfilled
            .write()
            .expect("key index backfilled lock poisoned")
            .insert(format!("{tenant}:{entity_type}"), key_set.to_string());
    }

    /// Compare durable projection rows with authoritative state rebuilt by event replay.
    pub async fn verify_query_projection_replay_parity(
        &self,
        tenant: &TenantId,
    ) -> Result<QueryProjectionReplayParityReport, String> {
        projection_backfill::verify_query_projection_replay_parity(
            self,
            tenant,
            None,
            None,
            "manual_full",
        )
        .await
    }

    /// Compare a bounded projection scope with authoritative event replay.
    pub async fn verify_query_projection_replay_parity_bounded(
        &self,
        tenant: &TenantId,
        entity_type: Option<&str>,
        entity_limit: Option<usize>,
        source: &str,
    ) -> Result<QueryProjectionReplayParityReport, String> {
        projection_backfill::verify_query_projection_replay_parity(
            self,
            tenant,
            entity_type,
            entity_limit,
            source,
        )
        .await
    }

    /// Hydrate actor state from the event store by spawning actors for all
    /// entities that have persisted events in this tenant.
    #[instrument(skip_all, fields(otel.name = "entity.hydrate_from_store", tenant = %tenant))]
    pub async fn hydrate_from_store(&self, tenant: &TenantId) {
        if let Some((store, _backend)) = self.event_journal() {
            match store.list_entity_ids(tenant.as_str()).await {
                Ok(entities) => {
                    let mut hydrated = 0usize;
                    for (entity_type, entity_id) in &entities {
                        if self
                            .ensure_entity_loaded(tenant, entity_type, entity_id)
                            .await
                        {
                            hydrated = hydrated.saturating_add(1);
                        }
                    }
                    // Eager hydration loaded every durable entity, so each
                    // observed type's index is complete — mark it hydrated so
                    // the first lazy list does not re-scan the store.
                    self.mark_types_hydrated(tenant, &entities);
                    tracing::info!(
                        tenant = %tenant,
                        count = hydrated,
                        discovered = entities.len(),
                        "hydrated entities from event store"
                    );
                    runtime_metrics::record_server_state_metrics(self);
                }
                Err(e) => {
                    tracing::error!(
                        tenant = %tenant,
                        error = %e,
                        "failed to hydrate from event store"
                    );
                }
            }
        }
    }
}
