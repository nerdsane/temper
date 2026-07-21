//! Entity indexes, durable backfill, and active counts.

use super::*;

impl ServerState {
    pub(super) fn touch_actor_access(&self, actor_key: &str) {
        if let Ok(mut last_accessed) = self.last_accessed.write() {
            last_accessed.insert(actor_key.to_string(), sim_now());
        }
    }

    /// Number of currently active (in-memory) entity actors.
    pub fn active_actor_count(&self) -> u64 {
        self.actor_registry
            .read()
            .map(|registry| registry.len() as u64)
            .unwrap_or(0)
    }

    /// Number of entities currently tracked by the in-memory entity index.
    pub fn active_entity_count(&self) -> u64 {
        self.entity_index
            .read()
            .map(|index| index.values().map(|ids| ids.len() as u64).sum())
            .unwrap_or(0)
    }

    /// Active entity counts grouped by tenant from the in-memory index.
    pub fn active_entity_counts_by_tenant(&self) -> BTreeMap<String, u64> {
        self.entity_index
            .read()
            .map(|index| {
                let mut counts = BTreeMap::new();
                for (index_key, ids) in index.iter() {
                    if let Some((tenant, _entity_type)) = index_key.split_once(':') {
                        *counts.entry(tenant.to_string()).or_insert(0) += ids.len() as u64;
                    }
                }
                counts
            })
            .unwrap_or_default()
    }

    /// Returns `true` when a tenant/entity_type has a registered spec.
    pub(crate) fn has_registered_spec(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<bool, String> {
        self.registry
            .read()
            .map(|registry| registry.get_spec(tenant, entity_type).is_some())
            .map_err(|e| format!("registry lock poisoned: {e}"))
    }

    /// Returns `true` when dispatch should be allowed for the entity type.
    ///
    /// This includes both tenant-scoped specs and legacy single-tenant
    /// transition tables.
    pub(crate) fn is_entity_type_governed(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<bool, String> {
        Ok(self.has_registered_spec(tenant, entity_type)?
            || self.transition_tables.contains_key(entity_type))
    }

    /// Declared `[[key]]` set for a `(tenant, entity_type)` (ADR-0153), resolved
    /// from the SAME sources dispatch uses: the per-tenant registry first — where
    /// runtime-installed os-app entities (File, Directory, SessionEntry, …) live —
    /// then the legacy single-tenant transition tables.
    ///
    /// The keyed read fast path MUST resolve keys through here. Reading
    /// `transition_tables` directly only sees the boot-time single-tenant set and
    /// silently omits every registry-installed entity, disabling the keyed path so
    /// point reads fall back to the budget-bounded scan and 413 at scale — the
    /// TemperFS root-directory failure in ARN-68.
    pub(crate) fn declared_keys_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Vec<temper_jit::table::types::DeclaredKey> {
        // Fail fast on a poisoned registry lock rather than silently falling through
        // to `transition_tables` — a silent fallback would re-introduce exactly the
        // ARN-68 bug (registry-installed keys not found → keyed path disabled → scan).
        {
            let registry = self.registry.read().expect("registry lock poisoned");
            if let Some(table) = registry.get_table(tenant, entity_type) {
                return table.keys.clone();
            }
        }
        self.transition_tables
            .get(entity_type)
            .map(|table| table.keys.clone())
            .unwrap_or_default()
    }

    /// The declared `[[vector]]` access paths for `(tenant, entity_type)` — the
    /// registry table first (covers os-app entities), the boot-time
    /// `transition_tables` as fallback (ADR-0155). Same registry-lock discipline as
    /// [`Self::declared_keys_for`]: fail fast on a poisoned lock rather than silently
    /// falling through. Empty when the type declares no vector path.
    pub(crate) fn declared_vectors_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Vec<temper_jit::table::types::DeclaredVector> {
        {
            let registry = self.registry.read().expect("registry lock poisoned");
            if let Some(table) = registry.get_table(tenant, entity_type) {
                return table.vectors.clone();
            }
        }
        self.transition_tables
            .get(entity_type)
            .map(|table| table.vectors.clone())
            .unwrap_or_default()
    }

    /// Load the current entity state and derive the Cedar resource view used
    /// for action authorization.
    pub(crate) async fn load_authz_resource_snapshot(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<AuthzResourceSnapshot, String> {
        let current_state = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await?;

        let mut resource_attrs = BTreeMap::new();
        resource_attrs.insert(
            "id".to_string(),
            serde_json::Value::String(entity_id.to_string()),
        );
        resource_attrs.insert(
            "status".to_string(),
            serde_json::Value::String(current_state.state.status.clone()),
        );
        if let serde_json::Value::Object(fields) = &current_state.state.fields {
            for (k, v) in fields {
                resource_attrs.insert(k.clone(), v.clone());
            }
        }

        let context_entities: Vec<temper_spec::automaton::ContextEntityDecl> = self
            .registry
            .read()
            .map_err(|e| format!("registry lock poisoned: {e}"))?
            .get_spec(tenant, entity_type)
            .map(|s| s.automaton.context_entities.clone())
            .unwrap_or_default();

        for ce in &context_entities {
            let target_id = current_state
                .state
                .fields
                .get(&ce.id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if !target_id.is_empty()
                && let Some(status) = self
                    .resolve_entity_status(tenant, &ce.entity_type, target_id)
                    .await
            {
                resource_attrs.insert(
                    format!("ctx_{}_status", ce.name),
                    serde_json::Value::String(status),
                );
            }
        }

        let has_spec = self.has_registered_spec(tenant, entity_type)?;
        resource_attrs.insert("has_spec".to_string(), serde_json::Value::Bool(has_spec));

        Ok(AuthzResourceSnapshot {
            current_state,
            resource_attrs,
        })
    }

    /// Mark every entity type observed in `entities` as fully hydrated from the
    /// durable store, so `list_entity_ids_lazy` serves it from the in-memory
    /// index without a redundant store scan. Used by the paths that load the
    /// authoritative set for a tenant (full and eager hydration).
    pub(super) fn mark_types_hydrated(&self, tenant: &TenantId, entities: &[(String, String)]) {
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

    /// Populate `entity_index` from the event store, eagerly spawning only
    /// entities whose specs declare state timeouts.
    ///
    /// This remains the memory-safe startup/list path for non-timed entities.
    /// Timed entities cannot remain fully lazy because their declared liveness
    /// transitions must be re-armed even when no request arrives after restart.
    #[instrument(skip_all, fields(otel.name = "entity.populate_index_from_store", tenant = %tenant))]
    pub async fn populate_index_from_store(&self, tenant: &TenantId) {
        self.ensure_registry_timeout_reconciliation_started();
        let Some((store, _backend)) = self.event_journal() else {
            return;
        };

        match store.list_entity_ids(tenant.as_str()).await {
            Ok(entities) => {
                let timed_entities = match self.registry.read() {
                    Ok(registry) => entities
                        .iter()
                        .filter(|(entity_type, _)| {
                            registry
                                .get_table(tenant, entity_type)
                                .or_else(|| self.transition_tables.get(entity_type).cloned())
                                .is_some_and(|table| !table.state_timeouts.is_empty())
                        })
                        .cloned()
                        .collect::<Vec<_>>(),
                    Err(_) => {
                        tracing::error!(
                            tenant = %tenant,
                            "timed entity startup skipped because the spec registry lock is poisoned"
                        );
                        Vec::new()
                    }
                };
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
                for (entity_type, entity_id) in &timed_entities {
                    if self
                        .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
                        .is_none()
                    {
                        tracing::error!(
                            tenant = %tenant,
                            entity_type,
                            entity_id,
                            "failed to spawn timed entity during startup reconciliation"
                        );
                    }
                }
                // A full-store scan is authoritative for every type it observed.
                self.mark_types_hydrated(tenant, &entities);
                tracing::info!(
                    tenant = %tenant,
                    count = entities.len(),
                    timed_spawned = timed_entities.len(),
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
}
