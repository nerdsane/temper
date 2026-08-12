//! Entity lifecycle methods for ServerState (spawn, query, delete, index).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use tracing::{Instrument, instrument};

use temper_observe::wide_event;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope, PersistenceError};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::dispatch::retry;
use super::{ServerState, projection_backfill};
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

/// Why an entity could not be created.
///
/// The create path used to return a bare `String`, so every failure — a key
/// collision, a store outage, a missing spec — reached the HTTP layer as one
/// undifferentiated message and left as `500 CreateError`. A caller could not
/// tell "change your request" from "try again in a second", which is how a
/// single declared-key collision turned into hundreds of identical retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityCreateError {
    /// A declared-key uniqueness constraint rejected the write: another entity
    /// of the same type already holds this key value (ADR-0153). Terminal —
    /// the same body collides with the same holder every time.
    DeclaredKeyConflict {
        /// The store's own rejection, which names the key and its holder.
        detail: String,
    },
    /// A dependency needed to create the entity was briefly unavailable.
    /// Worth retrying after a short pause.
    TransientDependency {
        /// The underlying failure, verbatim.
        detail: String,
    },
    /// Anything else. Retrying does not help.
    Internal {
        /// The underlying failure, verbatim.
        detail: String,
    },
}

impl EntityCreateError {
    /// Classify an actor failure surfaced by an `ask`.
    ///
    /// The classification rides on the error (see `InitFailureKind`); nothing
    /// here inspects message text to decide what kind of failure it was.
    pub(crate) fn from_actor_error(err: &temper_runtime::actor::ActorError) -> Self {
        use temper_runtime::actor::InitFailureKind;
        let detail = err.to_string();
        match err.init_failure_kind() {
            Some(InitFailureKind::Constraint) => Self::DeclaredKeyConflict { detail },
            Some(InitFailureKind::TransientDependency) => Self::TransientDependency { detail },
            Some(InitFailureKind::Defect) => Self::Internal { detail },
            None if err.is_transient() => Self::TransientDependency { detail },
            None => Self::Internal { detail },
        }
    }

    /// Classify a persistence failure raised on the data-only create fast path,
    /// which writes without an actor.
    pub(crate) fn from_persistence_error(context: &str, err: &PersistenceError) -> Self {
        match err {
            PersistenceError::DeclaredKeyConflict { .. } => Self::DeclaredKeyConflict {
                detail: err.to_string(),
            },
            other if other.is_transient() => Self::TransientDependency {
                detail: format!("{context}: {other}"),
            },
            other => Self::Internal {
                detail: format!("{context}: {other}"),
            },
        }
    }

    /// Stable label for metrics and logs.
    pub(crate) fn metric_label(&self) -> &'static str {
        match self {
            Self::DeclaredKeyConflict { .. } => "declared_key_conflict",
            Self::TransientDependency { .. } => "transient_dependency",
            Self::Internal { .. } => "internal",
        }
    }
}

impl std::fmt::Display for EntityCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeclaredKeyConflict { detail }
            | Self::TransientDependency { detail }
            | Self::Internal { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for EntityCreateError {}

impl From<EntityCreateError> for String {
    fn from(err: EntityCreateError) -> Self {
        err.to_string()
    }
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

/// Where a registration also listed the entity for collection queries:
/// the `{tenant}:{entity_type}` index key and the id inside it.
#[derive(Clone, Copy)]
pub(crate) struct IndexedAs<'a> {
    index_key: &'a str,
    entity_id: &'a str,
}

/// The in-memory bookkeeping that one entity spawn creates, and the only place
/// allowed to create or destroy it.
///
/// Three maps have to agree about every live actor: the actor registry a
/// request asks through, the entity index collection queries read, and the
/// access stamps passivation reads. Every registration and every removal
/// publishes its edits to all three inside a **single critical section held on
/// the actor-registry write lock** — the lock every reader already takes first
/// (`get_or_spawn_tenant_actor_with_fields`, `passivate_idle_actors`,
/// `observe::entities`). Two properties follow, and they are the point of this
/// type:
///
/// - no caller can observe an entity that is half-registered or half-removed
///   (registry entry gone while the index still lists it, or the reverse);
/// - a removal aimed at one incarnation cannot straddle a concurrent respawn
///   and delete the replacement's index entry or access stamp — the identity
///   check and the edits it guards are inside the same section.
///
/// It holds `Arc` clones rather than a `ServerState` reference because the
/// failed-init retraction runs inside the dead actor's own task, where there is
/// no `ServerState` to borrow.
#[derive(Clone)]
pub(crate) struct ActorDirectory {
    actor_registry: Arc<RwLock<BTreeMap<String, ActorRef<EntityMsg>>>>,
    entity_index: Arc<RwLock<BTreeMap<String, std::collections::BTreeSet<String>>>>,
    last_accessed: Arc<RwLock<BTreeMap<String, chrono::DateTime<chrono::Utc>>>>,
}

impl ActorDirectory {
    pub(crate) fn of(state: &ServerState) -> Self {
        Self {
            actor_registry: state.actor_registry.clone(),
            entity_index: state.entity_index.clone(),
            last_accessed: state.last_accessed.clone(),
        }
    }

    /// Register `spawn()`'s actor for `key` unless one is already registered.
    ///
    /// Returns the live actor and whether this call is the one that spawned it.
    /// The spawn happens under the lock, so the actor cannot fail and retract
    /// itself before the registration it is meant to undo is visible.
    fn get_or_register(
        &self,
        key: &str,
        indexed_as: IndexedAs<'_>,
        spawn: impl FnOnce() -> ActorRef<EntityMsg>,
    ) -> (ActorRef<EntityMsg>, bool) {
        let mut registry = self.write_registry();
        if let Some(existing) = registry.get(key) {
            let existing = existing.clone();
            self.stamp_access(key);
            return (existing, false);
        }
        let actor_ref = spawn();
        registry.insert(key.to_string(), actor_ref.clone());
        // Nested in the same order as every removal (registry → access stamp →
        // index) so the two never form a cycle, even though the registry lock
        // they both hold already makes them mutually exclusive.
        self.stamp_access(key);
        self.write_index()
            .entry(indexed_as.index_key.to_string())
            .or_default()
            .insert(indexed_as.entity_id.to_string());
        (actor_ref, true)
    }

    /// Remove the registration of one specific incarnation.
    ///
    /// A replacement spawned in the meantime carries a different `ActorId`, so
    /// the identity check makes this call either take that exact incarnation
    /// down whole or touch nothing at all.
    ///
    /// `unlist` additionally drops the entity-index entry. Pass it only when the
    /// entity provably does not exist; an actor that merely died (a failed
    /// hydration, an idle passivation) says nothing about whether the entity is
    /// there, and un-listing on that basis hides real data from collection
    /// queries.
    ///
    /// Returns whether anything was removed.
    fn unregister(
        &self,
        key: &str,
        incarnation: &temper_runtime::actor::ActorId,
        unlist: Option<IndexedAs<'_>>,
    ) -> bool {
        let mut registry = self.write_registry();
        match registry.get(key) {
            Some(current) if current.id().uid == incarnation.uid => {}
            _ => return false,
        }
        self.evict_locked(&mut registry, key, unlist);
        true
    }

    /// Remove whatever is registered for `key`, returning it so the caller can
    /// stop it once the lock is released.
    ///
    /// The delete path's counterpart to [`Self::unregister`]: no identity check
    /// (a delete evicts whichever incarnation is serving the entity) but the
    /// same single critical section, so a delete is no more observable
    /// half-applied than a retraction is.
    fn unregister_any(&self, key: &str, unlist: IndexedAs<'_>) -> Option<ActorRef<EntityMsg>> {
        let mut registry = self.write_registry();
        self.evict_locked(&mut registry, key, Some(unlist))
    }

    /// The removal itself. Takes the guard by `&mut` so the type system carries
    /// the proof that the whole eviction runs under the registry write lock.
    fn evict_locked(
        &self,
        registry: &mut BTreeMap<String, ActorRef<EntityMsg>>,
        key: &str,
        unlist: Option<IndexedAs<'_>>,
    ) -> Option<ActorRef<EntityMsg>> {
        let removed = registry.remove(key);
        self.write_last_accessed().remove(key);
        if let Some(IndexedAs {
            index_key,
            entity_id,
        }) = unlist
        {
            let mut index = self.write_index();
            if let Some(ids) = index.get_mut(index_key) {
                ids.remove(entity_id);
                if ids.is_empty() {
                    index.remove(index_key);
                }
            }
        }
        removed
    }

    /// Retract the spawn bookkeeping for an actor that permanently failed to
    /// initialize. Wired as the cell's `InitFailureObserver`, so it runs once
    /// per dead incarnation whether or not a caller is there to be told.
    fn retract_failed_init(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        failed: &temper_runtime::actor::ActorId,
        err: &temper_runtime::actor::ActorError,
    ) {
        let Some(kind) = err.init_failure_kind() else {
            return;
        };
        // A constraint rejection can only come from the bootstrap append, which
        // the actor performs solely when the entity has no events at all — so
        // the entity provably does not exist and must not be listed. Every other
        // init failure may belong to an entity that *does* exist and merely
        // failed to hydrate (an oversized replay tail, a store blip); un-listing
        // those would hide real data, so only the dead actor is retracted.
        let entity_provably_absent = kind == temper_runtime::actor::InitFailureKind::Constraint;
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let index_key = format!("{tenant}:{entity_type}");
        let unlist = entity_provably_absent.then_some(IndexedAs {
            index_key: &index_key,
            entity_id,
        });
        if !self.unregister(&key, failed, unlist) {
            return;
        }
        tracing::warn!(
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            failure = kind.as_str(),
            unlisted = entity_provably_absent,
            error = %err,
            "retracted the spawn of an actor that never initialized"
        );
        runtime_metrics::record_actor_directory_metrics(&self.actor_registry, &self.entity_index);
    }

    fn stamp_access(&self, key: &str) {
        self.write_last_accessed()
            .insert(key.to_string(), sim_now());
    }

    // A panic elsewhere must not strand entity bookkeeping: recovering the
    // guard keeps registration and removal working against a poisoned lock,
    // where bailing out would leave exactly the corpse this type exists to
    // prevent. The maps hold no invariant a panicking writer could have broken
    // mid-update — every edit here is a single map operation.
    fn write_registry(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, ActorRef<EntityMsg>>> {
        self.actor_registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_index(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, std::collections::BTreeSet<String>>> {
        self.entity_index
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_last_accessed(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, chrono::DateTime<chrono::Utc>>> {
        self.last_accessed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
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
        let key = format!("{tenant}:{entity_type}:{entity_id}");

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
        let table = {
            let reg = self.registry.read().unwrap();
            reg.get_table_live(tenant, entity_type)
        }
        .or_else(|| {
            // Legacy single-tenant: wrap the static Arc<TransitionTable> in a
            // new RwLock. Hot-swap doesn't apply to legacy mode, but the actor
            // API is uniform. One clone per entity spawn (cheap).
            self.transition_tables
                .get(entity_type)
                .map(|t| Arc::new(RwLock::new((**t).clone())))
        })?;

        // Build actor instance (spawn guarded below to avoid duplicate races).
        // ADR-0048 sub-decision 5: every actor gets the shared idempotency
        // cache so it can dedupe duplicate asks produced by retry storms.
        let tenant_blob_store = self.blob_store_for_tenant(tenant).ok();
        let snapshot_queue = self
            .snapshot_write_queue
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
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
            .with_snapshot_queue(snapshot_queue)
            .with_idempotency_cache(self.idempotency_cache.clone())
            .with_blob_store(tenant_blob_store.clone()),
            None => EntityActor::new(entity_type, entity_id, table, initial_fields)
                .with_tenant(tenant.as_str())
                .with_idempotency_cache(self.idempotency_cache.clone())
                .with_blob_store(tenant_blob_store),
        };

        // Slow-path: atomically re-check, spawn, and publish the bookkeeping
        // under one write lock. This prevents duplicate actors when concurrent
        // requests race to create the same (tenant, entity_type, entity_id)
        // key, and keeps the registry entry, the entity-index membership and
        // the access stamp indivisible — a retraction cannot land between them.
        let directory = ActorDirectory::of(self);
        let index_key = format!("{tenant}:{entity_type}");
        let on_init_failure: temper_runtime::actor::InitFailureObserver = {
            // The cell calls this when it permanently gives up on `pre_start`,
            // from its own task, before answering anyone — so the retraction
            // happens even when no caller is waiting (an abandoned request, an
            // actor spawned by a read that timed out) and is already done by
            // the time a caller is handed the failure.
            let directory = directory.clone();
            let tenant = tenant.clone();
            let entity_type = entity_type.to_string();
            let entity_id = entity_id.to_string();
            Arc::new(move |failed: &temper_runtime::actor::ActorId, err: &_| {
                directory.retract_failed_init(&tenant, &entity_type, &entity_id, failed, err);
            })
        };

        // The actor's own name is the registry key; `spawn` takes it by value,
        // so this clone replaces the allocation `spawn` would make anyway.
        let actor_name = key.clone();
        let indexed_as = IndexedAs {
            index_key: &index_key,
            entity_id,
        };
        let (actor_ref, spawned) = directory.get_or_register(&key, indexed_as, move || {
            self.actor_system
                .spawn_observing_init_failure(actor, actor_name, on_init_failure)
        });
        if spawned {
            runtime_metrics::record_server_state_metrics(self);
        }

        Some(actor_ref)
    }

    /// Count a create that failed before the entity existed, and hand the error
    /// back unchanged.
    ///
    /// A create that dies before its first event writes no trajectory row, so
    /// the per-entity-type success rate on `/observe/trajectories` stayed near
    /// 100% straight through an outage in which every create failed. This is
    /// the counter that makes a degraded entity type visible; it is exported
    /// both to OTLP and to `/observe/metrics`.
    fn record_create_failure(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        error: EntityCreateError,
    ) -> EntityCreateError {
        runtime_metrics::record_entity_spawn_failure(
            tenant.as_str(),
            entity_type,
            error.metric_label(),
        );
        self.metrics
            .record_entity_spawn_failure(entity_type, error.metric_label());
        error
    }

    /// Remove an entity from the index and actor registry.
    #[instrument(skip_all, fields(otel.name = "entity.remove_entity", tenant = %tenant, entity_type, entity_id))]
    pub fn remove_entity(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) {
        self.evict_entity(tenant, entity_type, entity_id, false);
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
        self.evict_entity(tenant, entity_type, entity_id, true);
    }

    /// Drop the registry entry, the entity-index membership and the access
    /// stamp together, optionally stopping the actor that was registered.
    ///
    /// One critical section for all three maps, the same one every other
    /// mutation of them uses — see [`ActorDirectory`]. The actor is stopped
    /// after the lock is released; `stop` is a mailbox send, and holding the
    /// registry across it would pin the lock behind a channel.
    fn evict_entity(&self, tenant: &TenantId, entity_type: &str, entity_id: &str, stop: bool) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
        let index_key = format!("{tenant}:{entity_type}");
        let removed = ActorDirectory::of(self).unregister_any(
            &actor_key,
            IndexedAs {
                index_key: &index_key,
                entity_id,
            },
        );
        if stop && let Some(actor_ref) = removed {
            let _ = actor_ref.stop();
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

    /// Check authorization for an action using the Cedar ABAC engine.
    ///
    /// Returns a typed [`AuthzDenial`] on failure, preserving the denial kind
    /// (policy denied, no matching permit, invalid principal, etc.).
    ///
    /// Accepts `BTreeMap` for DST compliance; converts at the authz boundary.
    pub fn authorize(
        &self,
        headers: &[(String, String)],
        action: &str,
        resource_type: &str,
        resource_attrs: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), AuthzDenial> {
        let ctx = SecurityContext::from_headers(headers);
        self.authorize_with_context(&ctx, action, resource_type, resource_attrs, "default")
    }

    /// Check authorization using a pre-built `SecurityContext`.
    ///
    /// Unlike [`authorize`] which builds the context from raw headers, this
    /// method accepts an already-constructed context enriched with agent
    /// identity and resource attributes.
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

    /// Create a new entity with initial fields and return its state.
    #[instrument(skip_all, fields(otel.name = "entity.get_or_create_tenant_entity", tenant = %tenant, entity_type, entity_id))]
    pub async fn get_or_create_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Result<EntityResponse, EntityCreateError> {
        let actor_ref = self
            .get_or_spawn_tenant_actor_with_fields(tenant, entity_type, entity_id, initial_fields)
            .ok_or_else(|| EntityCreateError::Internal {
                detail: format!(
                    "No transition table for tenant '{tenant}', entity type '{entity_type}'"
                ),
            })?;

        let policy = self.dispatch_retry_policy();
        let response = match retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::GetState,
            &policy,
        )
        .await
        .result
        {
            Ok(response) => response,
            Err(err) => {
                // An actor that could not initialize left nothing durable. Its
                // registry entry and index id are already retracted by the time
                // this error arrives — the cell does that before answering
                // anyone — so the retry respawns instead of asking a dead
                // mailbox. All that is left here is to count the failure per
                // entity type and report the classified cause.
                let create_error = self.record_create_failure(
                    tenant,
                    entity_type,
                    EntityCreateError::from_actor_error(&err),
                );
                tracing::error!(
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    failure = create_error.metric_label(),
                    error = %create_error,
                    "entity create failed before the entity existed"
                );
                return Err(create_error);
            }
        };

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
                return Err(self.record_create_failure(
                    tenant,
                    entity_type,
                    EntityCreateError::from_persistence_error(
                        "query projection write failed during create",
                        &e,
                    ),
                ));
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
    ) -> Result<Option<EntityResponse>, EntityCreateError> {
        if !initial_fields.is_object() {
            return Ok(None);
        }
        if self.is_pg_actor_backed(tenant, entity_type) {
            return Ok(None);
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
        let mut fields = initial_fields.clone();
        if let Some(obj) = fields.as_object_mut() {
            obj.entry("Id".to_string())
                .or_insert(serde_json::Value::String(entity_id.to_string()));
            obj.entry("Status".to_string())
                .or_insert(serde_json::Value::String(table.initial_state.clone()));
        }

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
        let payload = serde_json::to_value(&created).map_err(|e| EntityCreateError::Internal {
            detail: format!("failed to serialize Created event: {e}"),
        })?;
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
                    return Err(self.record_create_failure(
                        tenant,
                        entity_type,
                        EntityCreateError::from_persistence_error(
                            &format!(
                                "native data-only create failed for {entity_type}:{entity_id}"
                            ),
                            &e,
                        ),
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
                    return Err(self.record_create_failure(
                        tenant,
                        entity_type,
                        EntityCreateError::from_persistence_error(
                            &format!(
                                "failed to persist data-only Created event for {entity_type}:{entity_id}"
                            ),
                            &e,
                        ),
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
                return Err(self.record_create_failure(
                    tenant,
                    entity_type,
                    EntityCreateError::from_persistence_error(
                        "query projection write failed during data-only create",
                        &e,
                    ),
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
        let actor_ref = self
            .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let policy = self.dispatch_retry_policy();
        let fields_for_retry = fields;
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::UpdateFields {
                fields: fields_for_retry.clone(),
                replace,
            },
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor update failed: {e}"))?;

        if response.success
            && let Some(query_plane) = self.query_plane_store()
        {
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
        let actor_ref = self
            .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let policy = self.dispatch_retry_policy();
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::Delete,
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

            // Registry entry and access stamp go together: a respawn that lands
            // between them would keep its registry entry but lose its stamp,
            // and an actor with no stamp is never a passivation candidate
            // again. The identity check inside covers both.
            //
            // The entity index is deliberately left alone — passivation says
            // the actor is idle, not that the entity is gone, and the index is
            // what lets the next access lazy-spawn it.
            let removed = ActorDirectory::of(self).unregister(&actor_key, actor_ref.id(), None);

            if removed {
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
    pub fn enrich_metadata(&self, action_name: &str, hint: &str) {
        const AGENT_HINTS_BUDGET: usize = 1_000;
        let Ok(mut hints) = self.agent_hints.write() else {
            return;
        };
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

#[cfg(test)]
mod actor_directory_tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};

    use temper_jit::table::TransitionTable;
    use temper_runtime::ActorSystem;
    use temper_runtime::actor::ActorRef;

    use super::{ActorDirectory, IndexedAs};
    use crate::entity_actor::{EntityActor, EntityMsg};

    const DOC_IOA: &str = include_str!("../../../../test-fixtures/specs/keyed_doc.ioa.toml");

    fn directory() -> ActorDirectory {
        ActorDirectory {
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            last_accessed: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// A healthy actor, so the directory is exercised without an init failure
    /// racing the assertions.
    fn spawn_doc(system: &ActorSystem, entity_id: &str) -> ActorRef<EntityMsg> {
        let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
        system.spawn(
            EntityActor::new("Doc", entity_id, table, serde_json::json!({})).with_tenant("default"),
            format!("default:Doc:{entity_id}"),
        )
    }

    fn indexed_as<'a>(index_key: &'a str, entity_id: &'a str) -> IndexedAs<'a> {
        IndexedAs {
            index_key,
            entity_id,
        }
    }

    /// Registration publishes the registry entry, the index membership and the
    /// access stamp together.
    #[tokio::test]
    async fn registration_publishes_all_three_maps() {
        let system = ActorSystem::new("directory-register");
        let directory = directory();
        let key = "default:Doc:doc-1";

        let (_actor, spawned) =
            directory.get_or_register(key, indexed_as("default:Doc", "doc-1"), || {
                spawn_doc(&system, "doc-1")
            });

        assert!(spawned, "an empty directory spawns");
        assert!(directory.actor_registry.read().unwrap().contains_key(key));
        assert!(
            directory.entity_index.read().unwrap()["default:Doc"].contains("doc-1"),
            "a registered actor's entity must be listed for collection queries"
        );
        assert!(directory.last_accessed.read().unwrap().contains_key(key));

        // A second call finds the incumbent and does not spawn again.
        let (_again, spawned_again) =
            directory.get_or_register(key, indexed_as("default:Doc", "doc-1"), || {
                panic!("must not spawn a second incarnation for a registered key")
            });
        assert!(!spawned_again);
    }

    /// The whole retraction — identity check, registry removal, un-listing,
    /// access stamp — is one critical section on the registry write lock.
    ///
    /// Proof by obstruction: hold the entity index, then start a retraction
    /// that has to un-list. If it still holds the registry while it waits for
    /// the index, the registry write lock is unavailable to anyone else; if it
    /// released the registry first (removing the actor, then reaching for the
    /// index), the registry is free — and a concurrent caller could see the
    /// actor gone while the entity is still listed, or respawn and have its
    /// fresh index entry deleted by the tail of this cleanup.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retraction_holds_the_registry_lock_across_the_whole_cleanup() {
        let system = ActorSystem::new("directory-retract");
        let directory = directory();
        let key = "default:Doc:doc-2";

        let (actor, _) = directory.get_or_register(key, indexed_as("default:Doc", "doc-2"), || {
            spawn_doc(&system, "doc-2")
        });
        let incarnation = actor.id().clone();

        let index_held = directory.entity_index.write().unwrap();

        let retractor = {
            let directory = directory.clone();
            std::thread::spawn(move || {
                directory.unregister(
                    "default:Doc:doc-2",
                    &incarnation,
                    Some(indexed_as("default:Doc", "doc-2")),
                )
            })
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut registry_stayed_locked = false;
        while Instant::now() < deadline {
            if directory.actor_registry.try_write().is_err() {
                registry_stayed_locked = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(
            registry_stayed_locked,
            "the retraction reached the entity index without holding the actor \
             registry — the window where an entity is deregistered but still \
             listed is observable, and a respawn can slip into it"
        );

        drop(index_held);
        assert!(retractor.join().expect("the retraction thread finished"));
        assert!(!directory.actor_registry.read().unwrap().contains_key(key));
        assert!(
            !directory
                .entity_index
                .read()
                .unwrap()
                .contains_key("default:Doc"),
            "the last id of a type leaves no empty index bucket behind"
        );
        assert!(!directory.last_accessed.read().unwrap().contains_key(key));
    }

    /// The identity check guards every edit, not just the registry removal: a
    /// retraction aimed at a dead incarnation must leave a live successor's
    /// index entry and access stamp completely alone.
    #[tokio::test]
    async fn a_stale_retraction_cannot_touch_the_successor() {
        let system = ActorSystem::new("directory-stale");
        let directory = directory();
        let key = "default:Doc:doc-3";

        let (dead, _) = directory.get_or_register(key, indexed_as("default:Doc", "doc-3"), || {
            spawn_doc(&system, "doc-3")
        });
        let dead_incarnation = dead.id().clone();

        // The first incarnation goes away and a fresh one takes the key.
        assert!(directory.unregister(key, &dead_incarnation, None));
        let (successor, spawned) =
            directory.get_or_register(key, indexed_as("default:Doc", "doc-3"), || {
                spawn_doc(&system, "doc-3")
            });
        assert!(spawned, "the key was free, so this is a new incarnation");
        assert_ne!(successor.id().uid, dead_incarnation.uid);

        // The dead incarnation's cleanup arrives late.
        assert!(!directory.unregister(
            key,
            &dead_incarnation,
            Some(indexed_as("default:Doc", "doc-3"))
        ));

        assert!(
            directory.actor_registry.read().unwrap().contains_key(key),
            "the successor must still be registered"
        );
        assert!(
            directory.entity_index.read().unwrap()["default:Doc"].contains("doc-3"),
            "the successor's entity must still be listed — un-listing it would \
             hide a live entity from every collection query"
        );
        assert!(
            directory.last_accessed.read().unwrap().contains_key(key),
            "an actor with no access stamp is never passivated again"
        );
    }

    /// An unconditional eviction (the delete path) drops all three together and
    /// hands back the actor so the caller can stop it outside the lock.
    #[tokio::test]
    async fn eviction_drops_all_three_maps_and_returns_the_actor() {
        let system = ActorSystem::new("directory-evict");
        let directory = directory();
        let key = "default:Doc:doc-4";

        let (actor, _) = directory.get_or_register(key, indexed_as("default:Doc", "doc-4"), || {
            spawn_doc(&system, "doc-4")
        });

        let removed = directory
            .unregister_any(key, indexed_as("default:Doc", "doc-4"))
            .expect("the registered actor comes back so the caller can stop it");
        assert_eq!(removed.id().uid, actor.id().uid);

        assert!(directory.actor_registry.read().unwrap().is_empty());
        assert!(
            directory.entity_index.read().unwrap().is_empty(),
            "the deleted entity must not stay listed"
        );
        assert!(directory.last_accessed.read().unwrap().is_empty());

        assert!(
            directory
                .unregister_any(key, indexed_as("default:Doc", "doc-4"))
                .is_none(),
            "a second delete finds nothing and says so"
        );
    }
}

#[cfg(test)]
mod entity_create_error_tests {
    use super::EntityCreateError;
    use temper_runtime::actor::{ActorError, InitFailureKind};
    use temper_runtime::persistence::PersistenceError;

    #[test]
    fn a_constraint_init_failure_becomes_a_conflict() {
        let err = ActorError::init_failed(
            "failed to persist bootstrap Created event for File:fl-2: \
             duplicate declared key 'ws_path' for File: held by fl-1",
            InitFailureKind::Constraint,
        );
        match EntityCreateError::from_actor_error(&err) {
            EntityCreateError::DeclaredKeyConflict { detail } => {
                // The key and the holder are what the caller needs to act; they
                // must survive from the store to the response.
                assert!(detail.contains("ws_path"), "lost the key name: {detail}");
                assert!(detail.contains("fl-1"), "lost the holder id: {detail}");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    #[test]
    fn a_dependency_init_failure_becomes_retryable() {
        let err =
            ActorError::init_failed("storage error: reset", InitFailureKind::TransientDependency);
        assert!(matches!(
            EntityCreateError::from_actor_error(&err),
            EntityCreateError::TransientDependency { .. }
        ));
    }

    #[test]
    fn an_actor_error_that_is_not_an_init_failure_is_classified_by_transience() {
        // A dropped or timed-out ask is not an init failure, but a timeout is
        // still worth retrying and must not be reported as a hard error.
        assert!(matches!(
            EntityCreateError::from_actor_error(&ActorError::AskTimeout(
                std::time::Duration::from_secs(5)
            )),
            EntityCreateError::TransientDependency { .. }
        ));
        assert!(matches!(
            EntityCreateError::from_actor_error(&ActorError::MailboxFull),
            EntityCreateError::TransientDependency { .. }
        ));
        assert!(matches!(
            EntityCreateError::from_actor_error(&ActorError::Stopped),
            EntityCreateError::Internal { .. }
        ));
        assert!(matches!(
            EntityCreateError::from_actor_error(&ActorError::init_failed(
                "budget exceeded",
                InitFailureKind::Defect
            )),
            EntityCreateError::Internal { .. }
        ));
    }

    #[test]
    fn the_data_only_path_classifies_the_same_way() {
        // The fast path writes without an actor, so it classifies the typed
        // persistence error directly — same three outcomes, same wire contract.
        let conflict = PersistenceError::DeclaredKeyConflict {
            entity_type: "File".to_string(),
            key_name: "ws_path".to_string(),
            holder_id: "fl-1".to_string(),
        };
        match EntityCreateError::from_persistence_error("create failed", &conflict) {
            EntityCreateError::DeclaredKeyConflict { detail } => {
                assert!(detail.contains("ws_path") && detail.contains("fl-1"));
            }
            other => panic!("expected a conflict, got {other:?}"),
        }

        assert!(matches!(
            EntityCreateError::from_persistence_error(
                "create failed",
                &PersistenceError::Storage("connection reset".into())
            ),
            EntityCreateError::TransientDependency { .. }
        ));
        assert!(matches!(
            EntityCreateError::from_persistence_error(
                "create failed",
                &PersistenceError::Serialization("bad json".into())
            ),
            EntityCreateError::Internal { .. }
        ));
    }

    #[test]
    fn metric_labels_are_stable() {
        // Dashboards and monitors key off these strings.
        assert_eq!(
            EntityCreateError::DeclaredKeyConflict {
                detail: String::new()
            }
            .metric_label(),
            "declared_key_conflict"
        );
        assert_eq!(
            EntityCreateError::TransientDependency {
                detail: String::new()
            }
            .metric_label(),
            "transient_dependency"
        );
        assert_eq!(
            EntityCreateError::Internal {
                detail: String::new()
            }
            .metric_label(),
            "internal"
        );
    }

    #[test]
    fn display_keeps_the_cause_intact_for_string_callers() {
        // Callers that still take a String must not lose the reason.
        let err = EntityCreateError::TransientDependency {
            detail: "storage error: connection reset".to_string(),
        };
        assert_eq!(String::from(err.clone()), "storage error: connection reset");
        assert_eq!(err.to_string(), "storage error: connection reset");
    }
}
