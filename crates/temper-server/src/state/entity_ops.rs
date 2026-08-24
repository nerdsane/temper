//! Entity lifecycle methods for ServerState (spawn, query, delete, index).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use tracing::{Instrument, instrument};

use temper_observe::wide_event;
use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope, PersistenceError};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::dispatch::retry;
use super::{SCHEMA_PIN_MISMATCH_PREFIX, ServerState, projection_backfill};
use crate::entity_actor::{EntityActor, EntityEvent, EntityMsg, EntityResponse, EntityState};
use crate::events::EntityStateChange;
use crate::registry::{VerificationDetail, VerificationStatus};
use crate::runtime_metrics;
use crate::storage::DataOnlyCreateRecord;

fn actor_idle_timeout_secs() -> i64 {
    static ACTOR_IDLE_TIMEOUT: OnceLock<i64> = OnceLock::new();
    *ACTOR_IDLE_TIMEOUT.get_or_init(|| {
        std::env::var("TEMPER_ACTOR_IDLE_TIMEOUT") // determinism-ok: read once at startup
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(300)
    })
}

fn is_deleted_envelope(event: &PersistenceEnvelope) -> bool {
    if event.event_type == "Deleted" {
        return true;
    }
    event
        .payload
        .get("action")
        .and_then(serde_json::Value::as_str)
        == Some("Deleted")
}

pub(crate) fn validate_global_entity_id(entity_id: &str) -> Result<(), String> {
    if temper_runtime::persistence::schema_deployment::is_reserved_scoped_journal_entity_id(
        entity_id,
    ) {
        return Err(format!(
            "global entity ID '{entity_id}' uses the reserved scoped-journal identity form"
        ));
    }
    Ok(())
}

fn record_projection_update_started(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
) {
    crate::query_projection_metrics::record_update_started(
        tenant.as_str(),
        entity_type,
        operation,
        source,
    );
}

fn record_projection_update_success(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
    sequence_nr: u64,
    started_at: Instant,
) {
    let duration = started_at.elapsed();
    crate::query_projection_metrics::record_update_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "ok",
        duration,
    );
    crate::query_projection_metrics::record_update_end_to_end_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "ok",
        duration,
    );
    crate::query_projection_metrics::record_update_applied_sequence(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        sequence_nr,
    );
}

fn record_projection_update_error(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
    started_at: Instant,
) {
    let duration = started_at.elapsed();
    crate::query_projection_metrics::record_update_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "error",
        duration,
    );
    crate::query_projection_metrics::record_update_end_to_end_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "error",
        duration,
    );
    crate::query_projection_metrics::record_update_error(
        tenant.as_str(),
        entity_type,
        operation,
        source,
    );
}

/// Error returned when the verification gate blocks an operation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerificationGateError {
    /// The entity type that failed the gate.
    pub entity_type: String,
    /// Gate status: "pending", "running", or "failed".
    pub status: String,
    /// Human-readable message.
    pub message: String,
    /// Failed verification levels with details (only for "failed" status).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_levels: Option<Vec<FailedLevelInfo>>,
}

/// Information about a failed verification level.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedLevelInfo {
    /// Level name (e.g. "Level 2: Deterministic Simulation").
    pub level: String,
    /// Human-readable summary of the failure.
    pub summary: String,
    /// Detailed violation information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<VerificationDetail>>,
}

/// Snapshot of an entity as seen by Cedar authorization.
#[derive(Debug, Clone)]
pub(crate) struct AuthzResourceSnapshot {
    pub(crate) current_state: EntityResponse,
    pub(crate) resource_attrs: BTreeMap<String, serde_json::Value>,
}

impl ServerState {
    fn touch_actor_access(&self, actor_key: &str) {
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

        let resource_attrs = self
            .build_authz_resource_attrs(
                tenant,
                entity_type,
                entity_id,
                &current_state.state.status,
                &current_state.state.fields,
            )
            .await?;

        Ok(AuthzResourceSnapshot {
            current_state,
            resource_attrs,
        })
    }

    /// Build the Cedar resource view for a prospective entity representation.
    ///
    /// Mutation handlers use this after applying PATCH/PUT fields so policies
    /// evaluate the state that would be committed, including refreshed context
    /// entity status attributes.
    pub(crate) async fn build_authz_resource_attrs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
    ) -> Result<BTreeMap<String, serde_json::Value>, String> {
        let mut resource_attrs = BTreeMap::new();
        if let serde_json::Value::Object(fields) = fields {
            for (k, v) in fields {
                if !temper_spec::automaton::is_server_derived_field_name(k) {
                    resource_attrs.insert(k.clone(), v.clone());
                }
            }
        }

        for key in ["id", "Id"] {
            resource_attrs.insert(
                key.to_string(),
                serde_json::Value::String(entity_id.to_string()),
            );
        }
        for key in ["status", "Status"] {
            resource_attrs.insert(
                key.to_string(),
                serde_json::Value::String(status.to_string()),
            );
        }

        let context_entities: Vec<temper_spec::automaton::ContextEntityDecl> = self
            .registry
            .read()
            .map_err(|e| format!("registry lock poisoned: {e}"))?
            .get_spec(tenant, entity_type)
            .map(|s| s.automaton.context_entities.clone())
            .unwrap_or_default();

        for ce in &context_entities {
            let target_id = fields
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
        Ok(resource_attrs)
    }

    /// Return the spec-defined initial state used by a true entity create.
    pub(crate) fn initial_entity_status(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<String, String> {
        if let Some(table) = self
            .registry
            .read()
            .map_err(|error| format!("registry lock poisoned: {error}"))?
            .get_table(tenant, entity_type)
        {
            return Ok(table.initial_state.clone());
        }
        self.transition_tables
            .get(entity_type)
            .map(|table| table.initial_state.clone())
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })
    }

    /// Build trusted Cedar attributes for a durably absent create target.
    pub(crate) async fn build_create_authz_resource_attrs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: &serde_json::Value,
    ) -> Result<BTreeMap<String, serde_json::Value>, String> {
        let initial_status = self.initial_entity_status(tenant, entity_type)?;
        self.build_authz_resource_attrs(tenant, entity_type, entity_id, &initial_status, fields)
            .await
    }

    /// Mark every entity type observed in `entities` as fully hydrated from the
    /// durable store, so `list_entity_ids_lazy` serves it from the in-memory
    /// index without a redundant store scan. Used by the paths that load the
    /// authoritative set for a tenant (full and eager hydration).
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
    ) -> Result<super::QueryProjectionReplayParityReport, String> {
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
    ) -> Result<super::QueryProjectionReplayParityReport, String> {
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

    /// Get or spawn an entity actor (legacy single-tenant).
    #[deprecated(note = "Use `get_or_spawn_tenant_actor` with explicit tenant")]
    pub fn get_or_spawn_actor(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor(&TenantId::default(), entity_type, entity_id)
    }

    /// Get or spawn an entity actor for a specific tenant.
    pub fn get_or_spawn_tenant_actor(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor_with_fields(
            tenant,
            entity_type,
            entity_id,
            serde_json::json!({}),
        )
    }

    /// Get or spawn an entity actor with initial fields for a specific tenant.
    #[instrument(skip_all, fields(otel.name = "entity.get_or_spawn_tenant_actor_with_fields", tenant = %tenant, entity_type, entity_id))]
    pub fn get_or_spawn_tenant_actor_with_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor_with_fields_and_reference_evidence(
            tenant,
            entity_type,
            entity_id,
            initial_fields,
            BTreeMap::new(),
            None,
        )
    }

    /// Get or spawn an actor pinned to one exact immutable scoped bundle.
    pub fn get_or_spawn_scoped_actor_with_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        schema_pin: SchemaExecutionPin,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor_with_fields_and_reference_evidence(
            tenant,
            entity_type,
            entity_id,
            initial_fields,
            BTreeMap::new(),
            Some(schema_pin),
        )
    }

    fn get_or_spawn_tenant_actor_with_fields_and_reference_evidence(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        initial_reference_evidence: BTreeMap<String, bool>,
        schema_pin: Option<SchemaExecutionPin>,
    ) -> Option<ActorRef<EntityMsg>> {
        if schema_pin.is_none() && validate_global_entity_id(entity_id).is_err() {
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                "rejected global entity ID in reserved scoped-journal namespace"
            );
            return None;
        }
        let key = match schema_pin.as_ref() {
            Some(pin) => format!(
                "{tenant}:{entity_type}:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    entity_id, pin
                )
            ),
            None => format!("{tenant}:{entity_type}:{entity_id}"),
        };

        // Fast-path: check actor registry under read lock.
        {
            let registry = self.actor_registry.read().unwrap();
            if let Some(actor_ref) = registry.get(&key) {
                self.touch_actor_access(&key);
                return Some(actor_ref.clone());
            }
        }

        // Look up live transition table reference: try SpecRegistry first,
        // fall back to legacy map (wrapped in a fresh RwLock for compat).
        let table = if let Some(pin) = schema_pin.as_ref() {
            self.registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_scoped_table_at_digest(tenant, &pin.scope, &pin.bundle_digest, entity_type)
                .map(|table| Arc::new(RwLock::new((*table).clone())))
        } else {
            let live = {
                let reg = self
                    .registry
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                reg.get_table_live(tenant, entity_type)
            };
            live.or_else(|| {
                // Legacy single-tenant: wrap the static Arc<TransitionTable> in a
                // new RwLock. Hot-swap doesn't apply to legacy mode, but the actor
                // API is uniform. One clone per entity spawn (cheap).
                self.transition_tables
                    .get(entity_type)
                    .map(|t| Arc::new(RwLock::new((**t).clone())))
            })
        }?;

        // Build actor instance (spawn guarded below to avoid duplicate races).
        // ADR-0048 sub-decision 5: every actor gets the shared idempotency
        // cache so it can dedupe duplicate asks produced by retry storms.
        let tenant_blob_store = self.blob_store_for_tenant(tenant).ok();
        let snapshot_queue = self
            .snapshot_write_queue
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        let scoped_pin = schema_pin.clone();
        let actor = match self.event_journal() {
            Some((store, backend)) => EntityActor::with_persistence(
                entity_type,
                entity_id,
                table,
                initial_fields,
                store,
                backend,
            )
            .with_tenant(tenant.as_str())
            .with_initial_reference_evidence(initial_reference_evidence.clone())
            .with_snapshot_queue(snapshot_queue)
            .with_idempotency_cache(self.idempotency_cache.clone())
            .with_blob_store(tenant_blob_store.clone()),
            None => EntityActor::new(entity_type, entity_id, table, initial_fields)
                .with_tenant(tenant.as_str())
                .with_initial_reference_evidence(initial_reference_evidence)
                .with_idempotency_cache(self.idempotency_cache.clone())
                .with_blob_store(tenant_blob_store),
        };
        let actor = match schema_pin {
            Some(pin) => actor.with_schema_pin(pin),
            None => actor,
        };

        // Slow-path: atomically re-check and spawn under write lock.
        // This prevents duplicate actors when concurrent requests race to create
        // the same (tenant, entity_type, entity_id) key.
        let actor_ref = {
            let mut registry = self.actor_registry.write().unwrap();
            if let Some(existing) = registry.get(&key) {
                return Some(existing.clone());
            }
            let actor_ref = self.actor_system.spawn(actor, &key);
            registry.insert(key.clone(), actor_ref.clone());
            actor_ref
        };

        // Track in entity index for collection queries
        {
            let index_key = match scoped_pin.as_ref() {
                Some(pin) => format!(
                    "{tenant}:{entity_type}:{}",
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        "__index__",
                        pin,
                    )
                ),
                None => format!("{tenant}:{entity_type}"),
            };
            let mut index = self.entity_index.write().unwrap();
            index
                .entry(index_key)
                .or_default()
                .insert(entity_id.to_string());
        }
        self.touch_actor_access(&key);
        runtime_metrics::record_server_state_metrics(self);

        Some(actor_ref)
    }

    /// Sorted in-memory entity IDs for one immutable scoped bundle.
    pub fn list_scoped_entity_ids(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        schema_pin: &SchemaExecutionPin,
    ) -> Vec<String> {
        let key = format!(
            "{tenant}:{entity_type}:{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                "__index__",
                schema_pin,
            )
        );
        self.entity_index
            .read()
            .ok()
            .and_then(|index| index.get(&key).cloned())
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// Return one deterministic bounded page of scoped entity identities.
    pub(crate) async fn page_scoped_entity_ids(
        &self,
        tenant: &TenantId,
        entity_types: &[String],
        schema_pin: &SchemaExecutionPin,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, String> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut sorted_types = entity_types.to_vec();
        sorted_types.sort();
        if let Some((journal, _)) = self.event_journal() {
            let mut result = Vec::new();
            for entity_type in &sorted_types {
                if after.is_some_and(|(after_type, _)| entity_type.as_str() < after_type) {
                    continue;
                }
                let after_entity_id = after
                    .filter(|(after_type, _)| *after_type == entity_type)
                    .map(|(_, after_id)| after_id);
                let remaining = limit.saturating_sub(result.len());
                let ids = journal
                    .list_scoped_entity_ids_page(
                        tenant.as_str(),
                        entity_type,
                        &schema_pin.scope,
                        &schema_pin.bundle_digest,
                        after_entity_id,
                        remaining,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                result.extend(ids.into_iter().map(|id| (entity_type.clone(), id)));
                if result.len() == limit {
                    break;
                }
            }
            return Ok(result);
        }
        let index = self
            .entity_index
            .read()
            .expect("entity index lock poisoned");
        let mut result = Vec::new();
        for entity_type in &sorted_types {
            let key = format!(
                "{tenant}:{entity_type}:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    "__index__",
                    schema_pin,
                )
            );
            let Some(ids) = index.get(&key) else {
                continue;
            };
            for entity_id in ids {
                if after.is_some_and(|cursor| (entity_type.as_str(), entity_id.as_str()) <= cursor)
                {
                    continue;
                }
                result.push((entity_type.clone(), entity_id.clone()));
                if result.len() == limit {
                    return Ok(result);
                }
            }
        }
        Ok(result)
    }

    /// Stop and evict one exact scoped actor after a committed migration cutover.
    pub(crate) fn stop_and_remove_scoped_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: &SchemaExecutionPin,
    ) {
        let actor_key = format!(
            "{tenant}:{entity_type}:{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                entity_id, schema_pin,
            )
        );
        if let Ok(mut registry) = self.actor_registry.write()
            && let Some(actor_ref) = registry.remove(&actor_key)
        {
            let _ = actor_ref.stop();
        }
        if let Ok(mut last_accessed) = self.last_accessed.write() {
            last_accessed.remove(&actor_key);
        }
        let index_key = format!(
            "{tenant}:{entity_type}:{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                "__index__",
                schema_pin,
            )
        );
        if let Ok(mut index) = self.entity_index.write()
            && let Some(ids) = index.get_mut(&index_key)
        {
            ids.remove(entity_id);
        }
        runtime_metrics::record_server_state_metrics(self);
    }

    /// Remove an entity from the index and actor registry.
    #[instrument(skip_all, fields(otel.name = "entity.remove_entity", tenant = %tenant, entity_type, entity_id))]
    pub fn remove_entity(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");

        // Remove from actor registry
        {
            let mut registry = self.actor_registry.write().unwrap();
            registry.remove(&actor_key);
        }
        {
            let mut last_accessed = self.last_accessed.write().unwrap();
            last_accessed.remove(&actor_key);
        }

        // Remove from entity index
        {
            let index_key = format!("{tenant}:{entity_type}");
            let mut index = self.entity_index.write().unwrap();
            if let Some(ids) = index.get_mut(&index_key) {
                ids.remove(entity_id);
            }
        }
        runtime_metrics::record_server_state_metrics(self);
    }

    /// Stop and evict an entity actor plus its in-memory indexes.
    ///
    /// Used after an out-of-band durable append (for example, an atomic
    /// Composite batch) so the next read hydrates from the authoritative event
    /// journal instead of serving stale actor state.
    #[instrument(skip_all, fields(otel.name = "entity.stop_and_remove_entity", tenant = %tenant, entity_type, entity_id))]
    pub(crate) fn stop_and_remove_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");

        if let Ok(mut registry) = self.actor_registry.write()
            && let Some(actor_ref) = registry.remove(&actor_key)
        {
            let _ = actor_ref.stop();
        }
        if let Ok(mut last_accessed) = self.last_accessed.write() {
            last_accessed.remove(&actor_key);
        }
        if let Ok(mut index) = self.entity_index.write() {
            let index_key = format!("{tenant}:{entity_type}");
            if let Some(ids) = index.get_mut(&index_key) {
                ids.remove(entity_id);
            }
        }
        runtime_metrics::record_server_state_metrics(self);
    }

    /// List all entity IDs for a (tenant, entity_type) pair.
    #[instrument(skip_all, fields(otel.name = "entity.list_entity_ids", tenant = %tenant, entity_type))]
    pub fn list_entity_ids(&self, tenant: &TenantId, entity_type: &str) -> Vec<String> {
        let index_key = format!("{tenant}:{entity_type}");
        let index = self.entity_index.read().unwrap();
        index
            .get(&index_key)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Return at most `budget + 1` complete-index IDs, or `None` when obtaining
    /// a complete index would first require an unbounded durable-store scan.
    pub(crate) fn list_entity_ids_bounded(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        budget: usize,
    ) -> Option<Vec<String>> {
        let index_key = format!("{tenant}:{entity_type}");
        let complete = self.event_journal().is_none()
            || self
                .entity_index_hydrated
                .read()
                .expect("entity index hydrated lock poisoned")
                .contains(&index_key);
        if !complete {
            return None;
        }
        let index = self.entity_index.read().unwrap();
        Some(
            index
                .get(&index_key)
                .into_iter()
                .flat_map(|ids| ids.iter())
                .take(budget.saturating_add(1))
                .cloned()
                .collect(),
        )
    }

    /// Check authorization using a pre-built `SecurityContext`.
    ///
    /// This method accepts an already-constructed, credential-derived context
    /// enriched with agent identity and resource attributes. There is no
    /// header-based authorization entry point.
    ///
    /// Returns a typed [`AuthzDenial`] on failure, preserving the denial kind.
    ///
    /// Accepts `BTreeMap` for DST compliance; converts at the authz boundary.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(otel.name = "entity.authorize_with_context", action, resource_type))]
    pub fn authorize_with_context(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &BTreeMap<String, serde_json::Value>,
        tenant: &str,
    ) -> Result<(), AuthzDenial> {
        let attrs: std::collections::HashMap<_, _> = resource_attrs // determinism-ok: Cedar API requires HashMap
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(); // determinism-ok
        let authz_start = sim_now();
        let decision = self.authz.authorize_for_tenant_or_bypass(
            tenant,
            security_ctx,
            action,
            resource_type,
            &attrs,
        );
        let duration_ns = (sim_now() - authz_start)
            .num_nanoseconds()
            .unwrap_or(0)
            .max(0) as u64;
        let decision_str = match &decision {
            AuthzDecision::Allow { .. } => "Allow",
            AuthzDecision::Deny(_) => "Deny",
        };
        // Correlate the decision with the resource it governed and the request
        // that triggered it. `resource_attrs["id"]` is the Cedar resource id
        // every caller populates (see `resource_attrs_from_body`); the trace id
        // comes from the active span, which is the same request span the HTTP
        // handler and the dispatch both run under.
        let entity_id = resource_attrs
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let trace_id = crate::request_context::current_span_trace_context_ids()
            .map(|(trace_id, _span_id)| trace_id)
            .unwrap_or_default();
        let wide = wide_event::from_authz_decision(wide_event::AuthzDecisionInput {
            action,
            resource_type,
            entity_id,
            principal_kind: &format!("{:?}", security_ctx.principal.kind),
            decision: decision_str,
            duration_ns,
            tenant,
            trace_id: &trace_id,
        });
        wide_event::emit_span(&wide);
        wide_event::emit_metrics(&wide);
        match decision {
            AuthzDecision::Allow { .. } => Ok(()),
            AuthzDecision::Deny(denial) => Err(denial),
        }
    }

    /// Get the current state of an entity actor (legacy single-tenant).
    #[deprecated(note = "Use `get_tenant_entity_state` with explicit tenant")]
    pub async fn get_entity_state(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        self.get_tenant_entity_state(&TenantId::default(), entity_type, entity_id)
            .await
    }

    /// Get the current state of an entity actor for a specific tenant.
    #[instrument(skip_all, fields(otel.name = "entity.get_tenant_entity_state", tenant = %tenant, entity_type, entity_id))]
    pub async fn get_tenant_entity_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        // ADR-0048: retry transient ask failures (AskTimeout, MailboxFull)
        // so a single slow actor reply does not surface as HTTP 500.
        let policy = self.dispatch_retry_policy();
        retry::ask_with_backoff::<_, EntityResponse, _>(&actor_ref, || EntityMsg::GetState, &policy)
            .await
            .result
            .map_err(|e| format!("Actor query failed: {e}"))
    }

    /// Get exact state through one immutable scoped actor.
    pub async fn get_scoped_entity_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        let pin_exists = self
            .scoped_entity_pin_matches(tenant, entity_type, entity_id, &schema_pin)
            .await?;
        if !pin_exists {
            return Err(format!(
                "{SCHEMA_PIN_MISMATCH_PREFIX} scoped entity '{entity_type}/{entity_id}' has no durable pin for {}",
                schema_pin.bundle_digest
            ));
        }
        self.query_scoped_entity_state(tenant, entity_type, entity_id, schema_pin)
            .await
    }

    /// Read a scoped entity for dispatch, initializing it only at the active pin.
    pub(crate) async fn get_or_initialize_scoped_entity_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        self.validate_scoped_entity_pin_for_dispatch(tenant, entity_type, entity_id, &schema_pin)
            .await?;
        self.query_scoped_entity_state(tenant, entity_type, entity_id, schema_pin)
            .await
    }

    /// Validate the durable identity before any scoped dispatch selects a table or actor.
    pub(crate) async fn validate_scoped_entity_pin_for_dispatch(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: &SchemaExecutionPin,
    ) -> Result<(), String> {
        let pin_exists = self
            .scoped_entity_pin_matches(tenant, entity_type, entity_id, schema_pin)
            .await?;
        if !pin_exists {
            let pin_is_active = self
                .registry
                .read()
                .map_err(|_| "registry lock poisoned".to_string())?
                .active_scope_digest(tenant, &schema_pin.scope)
                == Some(schema_pin.bundle_digest.as_str());
            if !pin_is_active {
                return Err(format!(
                    "{SCHEMA_PIN_MISMATCH_PREFIX} scoped entity '{entity_type}/{entity_id}' has no durable pin for {}",
                    schema_pin.bundle_digest
                ));
            }
        }
        Ok(())
    }

    async fn query_scoped_entity_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_scoped_actor_with_fields(
                tenant,
                entity_type,
                entity_id,
                serde_json::json!({}),
                schema_pin,
            )
            .ok_or_else(|| {
                format!(
                    "No scoped transition table for tenant '{tenant}', entity type '{entity_type}'"
                )
            })?;
        let policy = self.dispatch_retry_policy();
        retry::ask_with_backoff::<_, EntityResponse, _>(&actor_ref, || EntityMsg::GetState, &policy)
            .await
            .result
            .map_err(|error| format!("Scoped actor query failed: {error}"))
    }

    /// Read an exact migration shadow journal without granting it dispatch authority.
    pub(crate) async fn get_scoped_migration_shadow_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        self.query_scoped_entity_state(tenant, entity_type, entity_id, schema_pin)
            .await
    }

    /// Enumerate exact-digest entity IDs within a caller-supplied work budget.
    pub(crate) async fn list_scoped_entity_ids_bounded(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        schema_pin: &SchemaExecutionPin,
        limit: usize,
    ) -> Result<Vec<String>, String> {
        if let Some((store, _)) = self.event_journal() {
            return store
                .list_scoped_entity_ids_page(
                    tenant.as_str(),
                    entity_type,
                    &schema_pin.scope,
                    &schema_pin.bundle_digest,
                    None,
                    limit,
                )
                .await
                .map_err(|error| error.to_string());
        }
        let prefix = format!("{tenant}:{entity_type}:");
        let suffix =
            temper_runtime::persistence::schema_deployment::scoped_journal_pin_suffix(schema_pin);
        let mut ids = self
            .actor_registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .filter_map(|key| {
                key.strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(&suffix))
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        ids.sort();
        ids.truncate(limit);
        Ok(ids)
    }

    /// Create a new entity with initial fields and return its state.
    #[instrument(skip_all, fields(otel.name = "entity.get_or_create_tenant_entity", tenant = %tenant, entity_type, entity_id))]
    pub async fn get_or_create_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        self.get_or_create_entity_with_schema_pin(
            tenant,
            entity_type,
            entity_id,
            initial_fields,
            None,
        )
        .await
    }

    /// Create or recover an entity under one exact immutable scoped bundle.
    pub async fn get_or_create_scoped_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        schema_pin: SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        self.get_or_create_entity_with_schema_pin(
            tenant,
            entity_type,
            entity_id,
            initial_fields,
            Some(schema_pin),
        )
        .await
    }

    async fn get_or_create_entity_with_schema_pin(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        schema_pin: Option<SchemaExecutionPin>,
    ) -> Result<EntityResponse, String> {
        let creating = match schema_pin.as_ref() {
            Some(pin) => {
                let exists = self
                    .scoped_entity_pin_matches(tenant, entity_type, entity_id, pin)
                    .await?;
                if !exists {
                    let active_digest = self
                        .registry
                        .read()
                        .map_err(|_| "registry lock poisoned".to_string())?
                        .active_scope_digest(tenant, &pin.scope)
                        .map(str::to_string);
                    if active_digest.as_deref() != Some(pin.bundle_digest.as_str()) {
                        return Err(format!(
                            "{SCHEMA_PIN_MISMATCH_PREFIX} bundle {} is not active for new entity '{entity_type}/{entity_id}'",
                            pin.bundle_digest
                        ));
                    }
                }
                !exists
            }
            None => {
                !self
                    .ensure_entity_loaded(tenant, entity_type, entity_id)
                    .await
            }
        };
        let initial_reference_evidence = if creating {
            let prepared_id = match schema_pin.as_ref() {
                Some(pin) => {
                    self.prepare_scoped_reference_contract_create(
                        tenant,
                        entity_type,
                        Some(entity_id),
                        &initial_fields,
                        pin,
                    )
                    .await?
                }
                None => {
                    self.prepare_reference_contract_create(
                        tenant,
                        entity_type,
                        Some(entity_id),
                        &initial_fields,
                    )
                    .await?
                }
            };
            debug_assert_eq!(prepared_id.as_deref(), Some(entity_id));
            self.resolve_reference_evidence(
                tenant,
                entity_type,
                entity_id,
                None,
                &initial_fields,
                schema_pin.as_ref(),
            )
            .await
        } else {
            BTreeMap::new()
        };
        let materialization_pin = schema_pin.clone();
        let actor_ref = self
            .get_or_spawn_tenant_actor_with_fields_and_reference_evidence(
                tenant,
                entity_type,
                entity_id,
                initial_fields,
                initial_reference_evidence,
                schema_pin,
            )
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let policy = self.dispatch_retry_policy();
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::GetState,
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor query failed: {e}"))?;

        // ADR-0178: the bootstrap Created event may carry a durable timeout
        // intent for a timed initial state. Materialize its pending lifecycle
        // immediately and wake the same fenced recovery owner used by
        // committed reactions.
        if creating && self.event_journal().is_some() && response.state.sequence_nr > 0 {
            let intents = self
                .materialize_committed_reaction_intents(
                    tenant,
                    entity_type,
                    entity_id,
                    response.state.sequence_nr,
                    materialization_pin.as_ref(),
                )
                .await
                .map_err(|error| error.to_string())?;
            if !intents.is_empty()
                && let Some(dispatcher) = self
                    .reaction_dispatcher
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            {
                dispatcher.notify_recovery(tenant);
            }
        }

        // Broadcast entity creation event for SSE subscribers
        let seq = self.next_entity_event_sequence(tenant.as_str(), entity_type, entity_id);
        let change = EntityStateChange {
            seq,
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: "Created".to_string(),
            status: response.state.status.clone(),
            tenant: tenant.to_string(),
            agent_id: None,
            session_id: None,
            intent: None,
            observation_metadata: None,
        };
        self.record_entity_observe_event_with_seq(
            tenant.as_str(),
            entity_type,
            entity_id,
            seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let _ = self.event_tx.send(change);

        if let Some(query_plane) = self.query_plane_store() {
            let status = response.state.status.clone();
            let fields = self.query_projection_fields(tenant, entity_type, &response.state.fields);
            let projected_state = self.query_projection_state(&response.state);
            let sequence_nr = response.state.sequence_nr;
            let operation = "upsert";
            let source = "create";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            if let Err(e) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &status,
                    &fields,
                    &projected_state,
                    sequence_nr,
                )
                .await
            {
                record_projection_update_error(
                    tenant,
                    entity_type,
                    operation,
                    source,
                    projection_started_at,
                );
                // Surface the projection failure instead of swallowing it.
                // Previously this was a `warn` followed by `Ok(response)`,
                // which produced HTTP 201 even when the entity wasn't
                // discoverable via $filter — silently corrupting any caller
                // that does write-then-read-back. Returning Err propagates
                // up through the OData write handler to a 5xx so the client
                // knows to retry. The event is already durable in the
                // events table; on retry the actor's idempotent UPSERT into
                // entity_catalog/entity_field_index will succeed (or fail
                // again loudly).
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during create"
                );
                return Err(format!("query projection write failed during create: {e}"));
            }
            record_projection_update_success(
                tenant,
                entity_type,
                operation,
                source,
                sequence_nr,
                projection_started_at,
            );
        }

        Ok(response)
    }

    /// Create a durable data-only entity without spawning an actor.
    ///
    /// This path is only eligible for transition tables with no rules. It
    /// preserves the event journal, projection acknowledgement, in-memory
    /// entity index, and observe/SSE event contracts used by the actor path.
    #[instrument(skip_all, fields(otel.name = "entity.create_data_only_tenant_entity_fast_path", tenant = %tenant, entity_type, entity_id))]
    pub async fn try_create_data_only_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Result<Option<EntityResponse>, String> {
        validate_global_entity_id(entity_id)?;
        if !initial_fields.is_object() {
            return Ok(None);
        }
        if self.is_pg_actor_backed(tenant, entity_type) {
            return Ok(None);
        }

        if !self
            .ensure_entity_loaded(tenant, entity_type, entity_id)
            .await
        {
            let prepared_id = self
                .prepare_reference_contract_create(
                    tenant,
                    entity_type,
                    Some(entity_id),
                    &initial_fields,
                )
                .await?;
            debug_assert_eq!(prepared_id.as_deref(), Some(entity_id));
        }

        let table = {
            let reg = self.registry.read().unwrap();
            reg.get_table_live(tenant, entity_type)
        }
        .or_else(|| {
            self.transition_tables
                .get(entity_type)
                .map(|t| Arc::new(RwLock::new((**t).clone())))
        });
        let Some(table_ref) = table else {
            return Ok(None);
        };
        let table = table_ref
            .read()
            .expect("transition table lock poisoned")
            .clone();
        if !table.rules.is_empty() {
            return Ok(None);
        }

        let Some((store, backend)) = self.event_journal() else {
            return Ok(None);
        };
        let Some(query_plane) = self.query_plane_store() else {
            return Ok(None);
        };

        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let initial_fields =
            crate::entity_actor::effects::sanitize_action_params(&initial_fields).into_owned();
        let mut fields = initial_fields.clone();
        crate::entity_actor::effects::canonicalize_entity_fields(
            &mut fields,
            entity_id,
            &table.initial_state,
        );

        let mut state = EntityState {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            status: table.initial_state.clone(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields,
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        };

        let created = EntityEvent {
            action: "Created".to_string(),
            from_status: String::new(),
            to_status: state.status.clone(),
            timestamp: sim_now(),
            params: initial_fields,
            idempotency_key: None,
        };
        let payload = serde_json::to_value(&created)
            .map_err(|e| format!("failed to serialize Created event: {e}"))?;
        let envelope = PersistenceEnvelope {
            sequence_nr: 1,
            event_type: created.action.clone(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: created.timestamp,
                actor_id: persistence_id.clone(),
            },
        };

        let projection_fields = self.query_projection_fields(tenant, entity_type, &state.fields);
        let mut created_projection_state = state.clone();
        created_projection_state.sequence_nr = 1;
        created_projection_state.push_event_bounded(created.clone());
        let projection_state = self.query_projection_state(&created_projection_state);
        if let Some(native_store) = self.data_only_create_store() {
            let operation = "native_create";
            let source = "data_only_create_fast_path";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            let native_span = tracing::info_span!(
                "entity.data_only_create_native_storage",
                otel.name = "entity.data_only_create_native_storage",
                tenant = %tenant,
                entity_type,
                entity_id,
            );
            match native_store
                .create_data_only_entity(DataOnlyCreateRecord {
                    tenant: tenant.as_str(),
                    entity_type,
                    entity_id,
                    status: &state.status,
                    fields: &projection_fields,
                    state: &projection_state,
                    event: &envelope,
                })
                .instrument(native_span)
                .await
            {
                Ok(new_seq) => {
                    state.sequence_nr = new_seq;
                    record_projection_update_success(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        state.sequence_nr,
                        projection_started_at,
                    );
                }
                Err(PersistenceError::ConcurrencyViolation { .. }) => {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    return Ok(None);
                }
                Err(e) => {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    tracing::error!(
                        error = %e,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "failed to update query projection during native data-only create fast path"
                    );
                    return Err(format!(
                        "native data-only create failed for {entity_type}:{entity_id}: {e}"
                    ));
                }
            }
        } else {
            let append_started_at = Instant::now(); // determinism-ok: production-only append wait metric
            let append_result = store
                .append(&persistence_id, state.sequence_nr, &[envelope])
                .await;
            runtime_metrics::record_event_store_append_wait(
                backend.as_str(),
                "append",
                append_started_at.elapsed(),
            );
            match append_result {
                Ok(new_seq) => {
                    state.sequence_nr = new_seq;
                }
                Err(PersistenceError::ConcurrencyViolation { .. }) => {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(format!(
                        "failed to persist data-only Created event for {entity_type}:{entity_id}: {e}"
                    ));
                }
            }

            let operation = "upsert";
            let source = "data_only_create_fast_path";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            if let Err(e) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &state.status,
                    &projection_fields,
                    &projection_state,
                    state.sequence_nr,
                )
                .await
            {
                record_projection_update_error(
                    tenant,
                    entity_type,
                    operation,
                    source,
                    projection_started_at,
                );
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during data-only create fast path"
                );
                return Err(format!(
                    "query projection write failed during data-only create: {e}"
                ));
            }
            record_projection_update_success(
                tenant,
                entity_type,
                operation,
                source,
                state.sequence_nr,
                projection_started_at,
            );
        }
        state.push_event_bounded(created);

        {
            let index_key = format!("{tenant}:{entity_type}");
            let mut index = self.entity_index.write().unwrap();
            index
                .entry(index_key)
                .or_default()
                .insert(entity_id.to_string());
        }
        runtime_metrics::record_server_state_metrics(self);

        let seq = self.next_entity_event_sequence(tenant.as_str(), entity_type, entity_id);
        let change = EntityStateChange {
            seq,
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: "Created".to_string(),
            status: state.status.clone(),
            tenant: tenant.to_string(),
            agent_id: None,
            session_id: None,
            intent: None,
            observation_metadata: None,
        };
        self.record_entity_observe_event_with_seq(
            tenant.as_str(),
            entity_type,
            entity_id,
            seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let _ = self.event_tx.send(change);

        Ok(Some(EntityResponse {
            success: true,
            state,
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        }))
    }

    /// Update fields on an existing entity.
    #[instrument(skip_all, fields(otel.name = "entity.update_tenant_entity_fields", tenant = %tenant, entity_type, entity_id))]
    pub async fn update_tenant_entity_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
    ) -> Result<EntityResponse, String> {
        self.update_tenant_entity_fields_if_sequence(
            tenant,
            entity_type,
            entity_id,
            fields,
            replace,
            None,
        )
        .await
    }

    /// Update fields only when the actor is still at `expected_sequence`.
    #[instrument(skip_all, fields(otel.name = "entity.update_tenant_entity_fields_if_sequence", tenant = %tenant, entity_type, entity_id))]
    pub async fn update_tenant_entity_fields_if_sequence(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        expected_sequence: Option<u64>,
    ) -> Result<EntityResponse, String> {
        self.update_entity_fields_with_schema_pin(
            tenant,
            entity_type,
            entity_id,
            fields,
            replace,
            expected_sequence,
            None,
            None,
        )
        .await
    }

    /// Update fields through one exact immutable scoped actor.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_scoped_entity_fields_if_sequence(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        expected_sequence: Option<u64>,
        schema_pin: SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        self.update_entity_fields_with_schema_pin(
            tenant,
            entity_type,
            entity_id,
            fields,
            replace,
            expected_sequence,
            Some(schema_pin),
            None,
        )
        .await
    }

    /// Update fields only if the global actor still matches the state Cedar authorized.
    pub(crate) async fn update_tenant_entity_fields_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        expected_precondition: String,
    ) -> Result<EntityResponse, String> {
        self.update_entity_fields_with_schema_pin(
            tenant,
            entity_type,
            entity_id,
            fields,
            replace,
            None,
            None,
            Some(expected_precondition),
        )
        .await
    }

    /// Update fields only if an immutable scoped actor still matches the state Cedar authorized.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn update_scoped_entity_fields_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        schema_pin: SchemaExecutionPin,
        expected_precondition: String,
    ) -> Result<EntityResponse, String> {
        self.update_entity_fields_with_schema_pin(
            tenant,
            entity_type,
            entity_id,
            fields,
            replace,
            None,
            Some(schema_pin),
            Some(expected_precondition),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_entity_fields_with_schema_pin(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        expected_sequence: Option<u64>,
        schema_pin: Option<SchemaExecutionPin>,
        expected_precondition: Option<String>,
    ) -> Result<EntityResponse, String> {
        if !fields.is_object() {
            return Err("entity field update must be a JSON object".to_string());
        }
        let reference_evidence = self
            .resolve_reference_evidence(
                tenant,
                entity_type,
                entity_id,
                None,
                &fields,
                schema_pin.as_ref(),
            )
            .await;
        let actor_ref = match schema_pin {
            Some(pin) => self.get_or_spawn_scoped_actor_with_fields(
                tenant,
                entity_type,
                entity_id,
                serde_json::json!({}),
                pin,
            ),
            None => self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id),
        }
        .ok_or_else(|| {
            format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
        })?;

        let mut policy = self.dispatch_retry_policy();
        if expected_precondition.is_some() || expected_sequence.is_some() {
            // A timed-out compare-and-set may already have committed. Retrying
            // the same precondition would turn that ambiguity into a false
            // conflict, so the caller must re-read and re-authorize instead.
            policy.max_attempts = 1;
        }
        let fields_for_retry = fields;
        let precondition_for_retry = expected_precondition;
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::UpdateFields {
                fields: fields_for_retry.clone(),
                replace,
                reference_evidence: reference_evidence.clone(),
                expected_sequence,
                expected_precondition: precondition_for_retry.clone(),
            },
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor update failed: {e}"))?;

        if !response.success {
            return Err(response
                .error
                .clone()
                .unwrap_or_else(|| "entity field update was rejected".to_string()));
        }

        if let Some(query_plane) = self.query_plane_store() {
            let status = response.state.status.clone();
            let fields = self.query_projection_fields(tenant, entity_type, &response.state.fields);
            let projected_state = self.query_projection_state(&response.state);
            let sequence_nr = response.state.sequence_nr;
            let operation = "upsert";
            let source = "field_update";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            if let Err(e) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &status,
                    &fields,
                    &projected_state,
                    sequence_nr,
                )
                .await
            {
                record_projection_update_error(
                    tenant,
                    entity_type,
                    operation,
                    source,
                    projection_started_at,
                );
                // Same reasoning as create: don't ack a write that won't be
                // visible via $filter. Propagate so OData returns 5xx and
                // clients can retry against the idempotent upsert.
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during field update"
                );
                return Err(format!("query projection write failed during update: {e}"));
            }
            record_projection_update_success(
                tenant,
                entity_type,
                operation,
                source,
                sequence_nr,
                projection_started_at,
            );
        }

        Ok(response)
    }

    /// Delete an entity.
    #[instrument(skip_all, fields(otel.name = "entity.delete_tenant_entity", tenant = %tenant, entity_type, entity_id))]
    pub async fn delete_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        let response = self
            .delete_entity_with_schema_pin(tenant, entity_type, entity_id, None, None)
            .await?;
        if response.success {
            Ok(response)
        } else {
            Err(response
                .error
                .clone()
                .unwrap_or_else(|| "entity delete was rejected".to_string()))
        }
    }

    /// Delete through one exact immutable scoped actor.
    pub async fn delete_scoped_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        self.delete_entity_with_schema_pin(tenant, entity_type, entity_id, Some(schema_pin), None)
            .await
    }

    /// Delete only if the global actor still matches the state Cedar authorized.
    pub(crate) async fn delete_tenant_entity_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        expected_authorization_precondition: String,
    ) -> Result<EntityResponse, String> {
        self.delete_entity_with_schema_pin(
            tenant,
            entity_type,
            entity_id,
            None,
            Some(expected_authorization_precondition),
        )
        .await
    }

    /// Delete only if a scoped actor still matches the state Cedar authorized.
    pub(crate) async fn delete_scoped_entity_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: SchemaExecutionPin,
        expected_authorization_precondition: String,
    ) -> Result<EntityResponse, String> {
        self.delete_entity_with_schema_pin(
            tenant,
            entity_type,
            entity_id,
            Some(schema_pin),
            Some(expected_authorization_precondition),
        )
        .await
    }

    async fn delete_entity_with_schema_pin(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        schema_pin: Option<SchemaExecutionPin>,
        expected_authorization_precondition: Option<String>,
    ) -> Result<EntityResponse, String> {
        let actor_ref = match schema_pin {
            Some(pin) => self.get_or_spawn_scoped_actor_with_fields(
                tenant,
                entity_type,
                entity_id,
                serde_json::json!({}),
                pin,
            ),
            None => self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id),
        }
        .ok_or_else(|| {
            format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
        })?;

        let mut policy = self.dispatch_retry_policy();
        if expected_authorization_precondition.is_some() {
            policy.max_attempts = 1;
        }
        let precondition_for_retry = expected_authorization_precondition;
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::Delete {
                expected_authorization_precondition: precondition_for_retry.clone(),
            },
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor delete failed: {e}"))?;

        if response.success {
            if let Some(query_plane) = self.query_plane_store() {
                let operation = "remove";
                let source = "delete";
                record_projection_update_started(tenant, entity_type, operation, source);
                let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
                if let Err(e) = query_plane
                    .remove_projection(tenant.as_str(), entity_type, entity_id)
                    .await
                {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    // Delete is idempotent against the projection: a stale row
                    // surviving in entity_field_index after the tombstone is a
                    // visibility leak, not data corruption. Log loudly so it's
                    // greppable but don't block the delete from completing —
                    // the actor and event journal already reflect the tombstone.
                    tracing::error!(
                        error = %e,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "failed to remove query projection during delete (stale projection row will linger until next successful upsert/delete)"
                    );
                } else {
                    record_projection_update_success(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        response.state.sequence_nr,
                        projection_started_at,
                    );
                }
            }

            // Tombstone persisted successfully; now it is safe to remove actor
            // and in-memory index entries.
            let _ = actor_ref.stop();
            self.remove_entity(tenant, entity_type, entity_id);
        }

        Ok(response)
    }

    /// Check if an entity exists in the index.
    pub fn entity_exists(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) -> bool {
        let index_key = format!("{tenant}:{entity_type}");
        let index = self.entity_index.read().unwrap();
        index
            .get(&index_key)
            .is_some_and(|ids| ids.contains(entity_id))
    }

    /// Ensure an entity is present in memory by lazily hydrating from the
    /// event store when needed.
    #[instrument(skip_all, fields(otel.name = "entity.ensure_entity_loaded", tenant = %tenant, entity_type, entity_id))]
    pub async fn ensure_entity_loaded(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> bool {
        if validate_global_entity_id(entity_id).is_err() {
            return false;
        }
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let journal = self.event_journal();

        if self.entity_exists(tenant, entity_type, entity_id) {
            let Some((store, _backend)) = journal.as_ref() else {
                return true;
            };

            let events = match store.read_events(&persistence_id, 0).await {
                Ok(events) if !events.is_empty() => events,
                _ => return true,
            };

            if events.last().is_some_and(is_deleted_envelope) {
                self.remove_entity(tenant, entity_type, entity_id);
                return false;
            }

            return true;
        }

        let Some((store, _backend)) = journal.as_ref() else {
            return false;
        };

        let events = match store.read_events(&persistence_id, 0).await {
            Ok(events) if !events.is_empty() => events,
            _ => return false,
        };

        if events.last().is_some_and(is_deleted_envelope) {
            self.remove_entity(tenant, entity_type, entity_id);
            return false;
        }

        let Some(actor_ref) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) else {
            return false;
        };

        let policy = self.dispatch_retry_policy();
        let outcome = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::GetState,
            &policy,
        )
        .await;
        match outcome.result {
            Ok(response) if response.state.status == "Deleted" => {
                let _ = actor_ref.stop();
                self.remove_entity(tenant, entity_type, entity_id);
                false
            }
            Ok(_) => true,
            Err(_) => {
                self.remove_entity(tenant, entity_type, entity_id);
                false
            }
        }
    }

    /// List entity IDs for a type, guaranteeing completeness against the
    /// durable event store.
    ///
    /// The in-memory index is served only once the type has been fully
    /// hydrated from the store. A non-empty index is NOT sufficient: lazily
    /// spawning a single actor inserts just that id, so trusting "non-empty
    /// means complete" lets a partial index hide durable entities, and a
    /// collection query then silently returns a partial set. When the type is
    /// not yet hydrated we scan the store once (which marks it complete) and
    /// then serve from the index on subsequent calls.
    #[instrument(skip_all, fields(otel.name = "entity.list_entity_ids_lazy", tenant = %tenant, entity_type))]
    pub async fn list_entity_ids_lazy(&self, tenant: &TenantId, entity_type: &str) -> Vec<String> {
        let index_key = format!("{tenant}:{entity_type}");
        let already_hydrated = self
            .entity_index_hydrated
            .read()
            .expect("entity index hydrated lock poisoned")
            .contains(&index_key);
        if already_hydrated {
            return self.list_entity_ids(tenant, entity_type);
        }

        // No durable journal to reconcile against: the in-memory index is all
        // there is, so return it as-is.
        if self.event_journal().is_none() {
            return self.list_entity_ids(tenant, entity_type);
        }

        self.populate_index_from_store_by_type(tenant, entity_type)
            .await;
        self.list_entity_ids(tenant, entity_type)
    }

    /// Passivate actors that have been idle longer than the configured timeout.
    ///
    /// Keeps `entity_index` entries intact so future accesses can lazy-spawn.
    #[instrument(skip_all, fields(otel.name = "entity.passivate_idle_actors"))]
    pub async fn passivate_idle_actors(&self) {
        let timeout_secs = actor_idle_timeout_secs();
        let cutoff = sim_now() - chrono::Duration::seconds(timeout_secs);

        let candidates: Vec<(String, ActorRef<EntityMsg>)> = {
            let Ok(registry) = self.actor_registry.read() else {
                return;
            };
            let Ok(last_accessed) = self.last_accessed.read() else {
                return;
            };
            registry
                .iter()
                .filter_map(|(key, actor_ref)| {
                    let last_seen = last_accessed.get(key)?;
                    if *last_seen <= cutoff {
                        Some((key.clone(), actor_ref.clone()))
                    } else {
                        None
                    }
                })
                .collect()
        };

        if candidates.is_empty() {
            return;
        }

        let mut passivated = 0usize;
        let policy = self.dispatch_retry_policy();
        let journal = self.event_journal();
        for (actor_key, actor_ref) in candidates {
            // ADR-0048: retry transient failures so passivation is not skipped
            // by a single AskTimeout under load.
            let snapshot_outcome = retry::ask_with_backoff::<_, EntityResponse, _>(
                &actor_ref,
                || EntityMsg::GetState,
                &policy,
            )
            .await;
            if let Some((store, _backend)) = journal.as_ref()
                && let Ok(response) = snapshot_outcome.result
                && response.state.sequence_nr > 0
            {
                // Snapshot excludes bounded in-memory recent event history.
                let mut snapshot_value = match serde_json::to_value(&response.state) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(actor_key = %actor_key, error = %e, "failed to encode snapshot value");
                        serde_json::Value::Null
                    }
                };
                if let Some(obj) = snapshot_value.as_object_mut() {
                    obj.remove("events");
                }
                if !snapshot_value.is_null()
                    && let Ok(snapshot_bytes) = serde_json::to_vec(&snapshot_value)
                    && let Err(e) = store
                        .save_snapshot(&actor_key, response.state.sequence_nr, &snapshot_bytes)
                        .await
                {
                    tracing::warn!(
                        actor_key = %actor_key,
                        seq = response.state.sequence_nr,
                        error = %e,
                        "failed to save snapshot during passivation"
                    );
                }
            }

            let _ = actor_ref.stop();

            let removed = {
                let Ok(mut registry) = self.actor_registry.write() else {
                    continue;
                };
                if registry
                    .get(&actor_key)
                    .is_some_and(|current| current.id().uid == actor_ref.id().uid)
                {
                    registry.remove(&actor_key);
                    true
                } else {
                    false
                }
            };

            if removed {
                if let Ok(mut last_accessed) = self.last_accessed.write() {
                    last_accessed.remove(&actor_key);
                }
                // Evict the state cache entry so stale status doesn't linger.
                if let Ok(mut cache) = self.entity_state_cache.lock() {
                    cache.pop(&actor_key);
                }
                passivated += 1;
            }
        }

        if passivated > 0 {
            runtime_metrics::record_server_state_metrics(self);
            tracing::info!(count = passivated, timeout_secs, "passivated idle actors");
        }
    }

    /// Update Agent.Hint annotations based on trajectory analysis.
    pub fn enrich_metadata(&self, tenant: &TenantId, action_name: &str, hint: &str) {
        const AGENT_HINTS_BUDGET: usize = 1_000;
        let Ok(mut all_hints) = self.agent_hints.write() else {
            return;
        };
        let hints = all_hints.entry(tenant.clone()).or_default();
        hints.insert(action_name.to_string(), hint.to_string());
        while hints.len() > AGENT_HINTS_BUDGET {
            let oldest_key = hints.iter().next().map(|(k, _)| k.clone());
            if let Some(k) = oldest_key {
                hints.remove(&k);
            } else {
                break;
            }
        }
    }

    /// Check the verification gate for a specific entity type.
    ///
    /// Returns `Ok(())` if the entity type is verified and operations are allowed.
    /// Returns `Err(VerificationGateError)` if operations should be blocked.
    ///
    /// Policy:
    /// - `None` → `Ok(())` (backward compat for legacy single-tenant without registry)
    /// - `Pending` → `Err("pending")` — verification hasn't started yet
    /// - `Running` → `Err("running")` — verification is in progress
    /// - `Completed(all_passed: true)` → `Ok(())`
    /// - `Completed(all_passed: false)` → `Err("failed")` with failed level details
    #[instrument(skip_all, fields(otel.name = "entity.check_verification_gate", tenant = %tenant, entity_type))]
    pub fn check_verification_gate(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<(), VerificationGateError> {
        let registry = self.registry.read().unwrap();

        // If there's no tenant config in the registry, this is a legacy
        // single-tenant setup — allow operations for backward compatibility.
        let Some(tenant_config) = registry.get_tenant(tenant) else {
            return Ok(());
        };

        // If the entity type doesn't exist in the tenant, there's nothing to gate.
        if !tenant_config.entities.contains_key(entity_type) {
            return Ok(());
        }

        match tenant_config.verification.get(entity_type) {
            None => Ok(()),
            Some(VerificationStatus::Pending) => Err(VerificationGateError {
                entity_type: entity_type.to_string(),
                status: "pending".to_string(),
                message: format!(
                    "Verification has not started for entity type '{entity_type}'. \
                     Waiting for verification cascade to begin."
                ),
                failed_levels: None,
            }),
            Some(VerificationStatus::Running) => Err(VerificationGateError {
                entity_type: entity_type.to_string(),
                status: "running".to_string(),
                message: format!(
                    "Verification is currently running for entity type '{entity_type}'. \
                     Please wait for the cascade to complete."
                ),
                failed_levels: None,
            }),
            Some(VerificationStatus::Completed(result) | VerificationStatus::Restored(result)) => {
                if result.all_passed {
                    Ok(())
                } else {
                    let failed_levels: Vec<FailedLevelInfo> = result
                        .levels
                        .iter()
                        .filter(|l| !l.passed)
                        .map(|l| FailedLevelInfo {
                            level: l.level.clone(),
                            summary: l.summary.clone(),
                            details: l.details.clone(),
                        })
                        .collect();
                    Err(VerificationGateError {
                        entity_type: entity_type.to_string(),
                        status: "failed".to_string(),
                        message: format!(
                            "Verification failed for entity type '{entity_type}'. \
                             Fix the spec and re-push."
                        ),
                        failed_levels: Some(failed_levels),
                    })
                }
            }
        }
    }
}

/// Resolve the current status of an entity.
///
/// Fast path: check the `entity_state_cache` (populated on every successful dispatch).
/// Slow path: fall back to `get_tenant_entity_state()` (async actor ask) and backfill cache.
impl ServerState {
    #[instrument(skip_all, fields(otel.name = "entity.resolve_entity_status", tenant = %tenant, entity_type, entity_id))]
    pub async fn resolve_entity_status(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<String> {
        // Fast path: check cache (LruCache::get requires &mut, so use Mutex).
        let cache_key = format!("{tenant}:{entity_type}:{entity_id}");
        if let Ok(mut cache) = self.entity_state_cache.lock()
            && let Some((status, _timestamp)) = cache.get(&cache_key)
        {
            return Some(status.clone());
        }

        // Slow path: actor ask + backfill
        if let Ok(response) = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            let status = response.state.status.clone();
            self.cache_entity_status(cache_key, status.clone());
            Some(status)
        } else {
            None
        }
    }
}

use temper_authz::{AuthzDecision, AuthzDenial, SecurityContext};
use temper_runtime::actor::ActorRef;
