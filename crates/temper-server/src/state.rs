//! Server state shared across all request handlers.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use sqlx::types::Json;
use temper_authz::{AuthzDecision, AuthzEngine, SecurityContext};
use temper_evolution::{PostgresRecordStore, RecordStore};
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::CsdlDocument;
use temper_store_postgres::PostgresEventStore;

use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use crate::event_store::ServerEventStore;
use crate::events::EntityStateChange;
use crate::reaction::ReactionDispatcher;
use crate::registry::{
    EntityVerificationResult, SpecRegistry, VerificationDetail, VerificationStatus,
};
use crate::webhooks::WebhookDispatcher;

/// A design-time event emitted during spec loading and verification.
///
/// These events are broadcast via SSE so the observe UI can show
/// verification progress in real time (design-time observation).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DesignTimeEvent {
    /// Event kind: "spec_loaded", "verify_started", "verify_level", "verify_done".
    pub kind: String,
    /// Entity type this event relates to.
    pub entity_type: String,
    /// Tenant this event relates to.
    pub tenant: String,
    /// Human-readable summary.
    pub summary: String,
    /// Verification level name (for "verify_level" events).
    pub level: Option<String>,
    /// Whether this level/entity passed (for "verify_level" and "verify_done" events).
    pub passed: Option<bool>,
    /// ISO-8601 timestamp when the event was created.
    pub timestamp: String,
    /// Step number in the workflow (1=loaded, 2=verify_started, 3-6=L0-L3, 7=done).
    pub step_number: Option<u8>,
    /// Total steps in the workflow (always 7 for verification).
    pub total_steps: Option<u8>,
}

/// Lightweight metrics collector for the /observe endpoints.
///
/// Uses atomic counters for totals and a `RwLock<BTreeMap>` for per-label
/// breakdowns. BTreeMap ensures deterministic iteration order (DST-safe).
pub struct MetricsCollector {
    /// Per-label transition counter: key = "entity_type:action:true|false".
    pub transitions: RwLock<BTreeMap<String, u64>>,
    /// Total successful + failed transitions.
    pub transitions_total: AtomicU64,
    /// Total failed transitions (guard not met, unknown action).
    pub errors_total: AtomicU64,
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsCollector {
    /// Create a new, empty collector.
    pub fn new() -> Self {
        Self {
            transitions: RwLock::new(BTreeMap::new()),
            transitions_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
        }
    }

    /// Record a transition result.
    pub fn record_transition(&self, entity_type: &str, action: &str, success: bool) {
        let label = if success {
            format!("{entity_type}:{action}:true")
        } else {
            format!("{entity_type}:{action}:false")
        };
        if let Ok(mut map) = self.transitions.write() {
            *map.entry(label).or_insert(0) += 1;
        }
        self.transitions_total.fetch_add(1, Ordering::Relaxed);
        if !success {
            self.errors_total.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Maximum number of trajectory entries retained in the bounded log.
const TRAJECTORY_LOG_CAPACITY: usize = 10_000;
/// Maximum number of design-time events retained in memory.
const DESIGN_TIME_LOG_CAPACITY: usize = 10_000;

/// A single trajectory entry recording the outcome of a dispatched action.
///
/// Captures both successful transitions and failed intents (guard rejection,
/// unknown action, actor timeout) so the Evolution Engine can analyse gaps.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrajectoryEntry {
    /// ISO-8601 timestamp (DST-safe: uses sim_now()).
    pub timestamp: String,
    /// Tenant that owns the entity.
    pub tenant: String,
    /// Entity type targeted by the action.
    pub entity_type: String,
    /// Entity ID targeted by the action.
    pub entity_id: String,
    /// Action name that was dispatched.
    pub action: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// Entity status before the action (if known).
    pub from_status: Option<String>,
    /// Entity status after the action (if known).
    pub to_status: Option<String>,
    /// Error description for failed intents.
    pub error: Option<String>,
}

/// Bounded, append-only trajectory log.
///
/// Uses `VecDeque` with a fixed capacity. When the log is full, the oldest
/// entry is evicted (ring-buffer semantics). Protected by `RwLock` for
/// concurrent access from multiple request handlers.
pub struct TrajectoryLog {
    /// The bounded deque of trajectory entries.
    entries: VecDeque<TrajectoryEntry>,
    /// Maximum capacity.
    capacity: usize,
}

impl TrajectoryLog {
    /// Create a new trajectory log with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Append an entry, evicting the oldest if at capacity.
    pub fn push(&mut self, entry: TrajectoryEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Read-only access to all entries (oldest first).
    pub fn entries(&self) -> &VecDeque<TrajectoryEntry> {
        &self.entries
    }
}

/// Shared state for the Temper HTTP server.
#[derive(Clone)]
pub struct ServerState {
    /// The actor system for spawning and managing entity actors.
    pub actor_system: Arc<ActorSystem>,
    /// Parsed CSDL document describing the entity model (legacy single-tenant).
    pub csdl: Arc<CsdlDocument>,
    /// Raw CSDL XML string for serving via `$metadata` (legacy single-tenant).
    pub csdl_xml: Arc<String>,
    /// Maps entity set names to entity type names (legacy single-tenant).
    pub entity_set_map: Arc<BTreeMap<String, String>>,
    /// Transition table per entity type (legacy single-tenant).
    pub transition_tables: Arc<BTreeMap<String, Arc<TransitionTable>>>,
    /// Live actor registry: actor_key -> ActorRef.
    pub actor_registry: Arc<RwLock<BTreeMap<String, ActorRef<EntityMsg>>>>,
    /// Optional runtime event store backend for persistence.
    pub event_store: Option<Arc<ServerEventStore>>,
    /// Agent hints learned from trajectory analysis, keyed by action name.
    pub agent_hints: Arc<RwLock<BTreeMap<String, String>>>,
    /// Cedar ABAC authorization engine.
    pub authz: Arc<AuthzEngine>,
    /// Multi-tenant specification registry (shared, mutable for live registration).
    pub registry: Arc<RwLock<SpecRegistry>>,
    /// Index of entity IDs per (tenant:entity_type) for collection queries.
    pub entity_index: Arc<RwLock<BTreeMap<String, BTreeSet<String>>>>,
    /// Broadcast channel for entity state change events (SSE subscriptions).
    pub event_tx: Arc<tokio::sync::broadcast::Sender<EntityStateChange>>,
    /// Server start time (DST-safe: uses sim_now()).
    pub start_time: chrono::DateTime<chrono::Utc>,
    /// Metrics collector for the /observe endpoints.
    pub metrics: Arc<MetricsCollector>,
    /// Bounded trajectory log for failed intent analysis and Evolution Engine.
    pub trajectory_log: Arc<RwLock<TrajectoryLog>>,
    /// In-memory evolution record store (O/P/A/D/I records).
    pub record_store: Arc<RecordStore>,
    /// Optional Postgres evolution record store (source of truth when configured).
    pub pg_record_store: Option<Arc<PostgresRecordStore>>,
    /// Optional reaction dispatcher for cross-entity coordination.
    pub reaction_dispatcher: Option<Arc<ReactionDispatcher>>,
    /// Optional webhook dispatcher for external system notifications.
    pub webhook_dispatcher: Option<Arc<WebhookDispatcher>>,
    /// Broadcast channel for design-time events (spec loading, verification progress).
    pub design_time_tx: Arc<tokio::sync::broadcast::Sender<DesignTimeEvent>>,
    /// In-memory log of design-time events for workflow history (append-only, bounded).
    pub design_time_log: Arc<RwLock<Vec<DesignTimeEvent>>>,
    /// Cache of entity current state, updated on every state change broadcast.
    /// Key: "{tenant}:{entity_type}:{entity_id}", Value: (current_state, last_updated).
    #[allow(clippy::type_complexity)]
    pub entity_state_cache: Arc<RwLock<BTreeMap<String, (String, chrono::DateTime<chrono::Utc>)>>>,
}

impl ServerState {
    /// Create ServerState from CSDL XML and optional specification sources.
    pub fn new(system: ActorSystem, csdl: CsdlDocument, csdl_xml: String) -> Self {
        let mut entity_set_map = BTreeMap::new();
        for schema in &csdl.schemas {
            for container in &schema.entity_containers {
                for entity_set in &container.entity_sets {
                    let type_name = entity_set
                        .entity_type
                        .rsplit('.')
                        .next()
                        .unwrap_or(&entity_set.entity_type);
                    entity_set_map.insert(entity_set.name.clone(), type_name.to_string());
                }
            }
        }

        let (event_tx, _) = tokio::sync::broadcast::channel(256);
        let (design_time_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            actor_system: Arc::new(system),
            csdl: Arc::new(csdl),
            csdl_xml: Arc::new(csdl_xml),
            entity_set_map: Arc::new(entity_set_map),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            event_store: None,
            agent_hints: Arc::new(RwLock::new(BTreeMap::new())),
            authz: Arc::new(AuthzEngine::permissive()),
            registry: Arc::new(RwLock::new(SpecRegistry::new())),
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            event_tx: Arc::new(event_tx),
            start_time: sim_now(),
            metrics: Arc::new(MetricsCollector::new()),
            trajectory_log: Arc::new(RwLock::new(TrajectoryLog::new(TRAJECTORY_LOG_CAPACITY))),
            record_store: Arc::new(RecordStore::new()),
            pg_record_store: None,
            reaction_dispatcher: None,
            webhook_dispatcher: None,
            design_time_tx: Arc::new(design_time_tx),
            design_time_log: Arc::new(RwLock::new(Vec::new())),
            entity_state_cache: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Create ServerState with I/O Automaton TOML specs for transition table resolution.
    pub fn with_specs(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
    ) -> Self {
        let mut state = Self::new(system, csdl, csdl_xml);
        let mut tables = BTreeMap::new();
        for (entity_type, ioa_source) in &ioa_sources {
            let table = TransitionTable::from_ioa_source(ioa_source);
            tables.insert(entity_type.clone(), Arc::new(table));
        }
        state.transition_tables = Arc::new(tables);
        state
    }

    /// Create ServerState with specs AND Postgres persistence.
    pub fn with_persistence(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
        store: PostgresEventStore,
    ) -> Self {
        let mut state = Self::with_specs(system, csdl, csdl_xml, ioa_sources);
        state.event_store = Some(Arc::new(ServerEventStore::Postgres(store)));
        state
    }

    /// Create ServerState with specs and an explicit runtime event store.
    pub fn with_event_store(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
        store: ServerEventStore,
    ) -> Self {
        let mut state = Self::with_specs(system, csdl, csdl_xml, ioa_sources);
        state.event_store = Some(Arc::new(store));
        state
    }

    /// Create ServerState from a multi-tenant [`SpecRegistry`].
    pub fn from_registry(system: ActorSystem, registry: SpecRegistry) -> Self {
        Self::from_registry_shared(system, Arc::new(RwLock::new(registry)))
    }

    /// Create ServerState from a shared, mutable [`SpecRegistry`].
    ///
    /// Use this when the registry must be shared with another component
    /// (e.g. `PlatformState`) so that writes are visible to dispatch.
    pub fn from_registry_shared(system: ActorSystem, registry: Arc<RwLock<SpecRegistry>>) -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(256);
        let (design_time_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            actor_system: Arc::new(system),
            csdl: Arc::new(CsdlDocument {
                version: "4.0".into(),
                schemas: vec![],
            }),
            csdl_xml: Arc::new(String::new()),
            entity_set_map: Arc::new(BTreeMap::new()),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            event_store: None,
            agent_hints: Arc::new(RwLock::new(BTreeMap::new())),
            authz: Arc::new(AuthzEngine::permissive()),
            registry,
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            event_tx: Arc::new(event_tx),
            start_time: sim_now(),
            metrics: Arc::new(MetricsCollector::new()),
            trajectory_log: Arc::new(RwLock::new(TrajectoryLog::new(TRAJECTORY_LOG_CAPACITY))),
            record_store: Arc::new(RecordStore::new()),
            pg_record_store: None,
            reaction_dispatcher: None,
            webhook_dispatcher: None,
            design_time_tx: Arc::new(design_time_tx),
            design_time_log: Arc::new(RwLock::new(Vec::new())),
            entity_state_cache: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Attach a reaction dispatcher for cross-entity coordination.
    pub fn with_reaction_dispatcher(mut self, dispatcher: Arc<ReactionDispatcher>) -> Self {
        self.reaction_dispatcher = Some(dispatcher);
        self
    }

    /// Attach a webhook dispatcher for external system notifications.
    pub fn with_webhook_dispatcher(mut self, dispatcher: Arc<WebhookDispatcher>) -> Self {
        self.webhook_dispatcher = Some(dispatcher);
        self
    }

    /// Attach a Postgres-backed evolution record store.
    pub fn with_pg_record_store(mut self, store: PostgresRecordStore) -> Self {
        self.pg_record_store = Some(Arc::new(store));
        self
    }

    /// Upsert a spec source into Postgres when persistence is configured.
    pub async fn upsert_spec_source(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
    ) -> Result<(), String> {
        let Some(pool) = self
            .event_store
            .as_ref()
            .and_then(|store| store.postgres_pool())
        else {
            return Ok(());
        };
        sqlx::query(
            "INSERT INTO specs \
             (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, updated_at) \
             VALUES ($1, $2, $3, $4, 1, false, 'pending', now()) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = EXCLUDED.ioa_source, \
                 csdl_xml = EXCLUDED.csdl_xml, \
                 version = specs.version + 1, \
                 verified = false, \
                 verification_status = 'pending', \
                 levels_passed = NULL, \
                 levels_total = NULL, \
                 verification_result = NULL, \
                 updated_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(ioa_source)
        .bind(csdl_xml)
        .execute(pool)
        .await
        .map_err(|e| format!("failed to upsert spec {tenant}/{entity_type} in postgres: {e}"))?;
        Ok(())
    }

    /// Persist verification summary for a spec when Postgres is configured.
    pub async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        status: &str,
        result: Option<&EntityVerificationResult>,
    ) -> Result<(), String> {
        let Some(pool) = self
            .event_store
            .as_ref()
            .and_then(|store| store.postgres_pool())
        else {
            return Ok(());
        };
        let (verified, levels_passed, levels_total, verification_result) = match result {
            Some(r) => {
                let passed = r.levels.iter().filter(|l| l.passed).count() as i32;
                let total = r.levels.len() as i32;
                let as_json = serde_json::to_value(r).ok();
                (r.all_passed, Some(passed), Some(total), as_json)
            }
            None => (false, None, None, None),
        };
        sqlx::query(
            "UPDATE specs SET \
                 verified = $3, \
                 verification_status = $4, \
                 levels_passed = $5, \
                 levels_total = $6, \
                 verification_result = $7, \
                 updated_at = now() \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(verified)
        .bind(status)
        .bind(levels_passed)
        .bind(levels_total)
        .bind(verification_result.map(Json))
        .execute(pool)
        .await
        .map_err(|e| {
            format!(
                "failed to persist spec verification status for {tenant}/{entity_type} ({status}): {e}"
            )
        })?;
        Ok(())
    }

    /// Append to in-memory design-time log with bounded capacity.
    pub fn push_design_time_event(&self, event: DesignTimeEvent) {
        if let Ok(mut log) = self.design_time_log.write() {
            if log.len() >= DESIGN_TIME_LOG_CAPACITY {
                // Keep the newest events; evict oldest one.
                let _ = log.remove(0);
            }
            log.push(event);
        }
    }

    /// Broadcast and persist a design-time event.
    pub async fn emit_design_time_event(&self, event: DesignTimeEvent) -> Result<(), String> {
        let Some(pool) = self
            .event_store
            .as_ref()
            .and_then(|store| store.postgres_pool())
        else {
            let _ = self.design_time_tx.send(event.clone());
            self.push_design_time_event(event);
            return Ok(());
        };
        let created_at = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| sim_now());
        sqlx::query(
            "INSERT INTO design_time_events \
             (kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&event.kind)
        .bind(&event.entity_type)
        .bind(&event.tenant)
        .bind(&event.summary)
        .bind(event.level.as_deref())
        .bind(event.passed)
        .bind(event.step_number.map(i16::from))
        .bind(event.total_steps.map(i16::from))
        .bind(created_at)
        .execute(pool)
        .await
        .map_err(|e| {
            format!(
                "failed to persist design-time event {} for {}/{}: {e}",
                event.kind, event.tenant, event.entity_type
            )
        })?;
        let _ = self.design_time_tx.send(event.clone());
        self.push_design_time_event(event);
        Ok(())
    }

    /// Persist a trajectory entry when Postgres is configured.
    pub async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let Some(pool) = self
            .event_store
            .as_ref()
            .and_then(|store| store.postgres_pool())
        else {
            return Ok(());
        };
        let created_at = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| sim_now());
        sqlx::query(
            "INSERT INTO trajectories \
             (tenant, entity_type, entity_id, action, success, from_status, to_status, error, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&entry.tenant)
        .bind(&entry.entity_type)
        .bind(&entry.entity_id)
        .bind(&entry.action)
        .bind(entry.success)
        .bind(entry.from_status.as_deref())
        .bind(entry.to_status.as_deref())
        .bind(entry.error.as_deref())
        .bind(created_at)
        .execute(pool)
        .await
        .map_err(|e| {
            format!(
                "failed to persist trajectory entry for {}/{}/{} action {}: {e}",
                entry.tenant, entry.entity_type, entry.entity_id, entry.action
            )
        })?;
        Ok(())
    }

    /// Hydrate actor state from the event store by spawning actors for all
    /// entities that have persisted events in this tenant.
    pub async fn hydrate_from_store(&self, tenant: &TenantId) {
        if let Some(ref store) = self.event_store {
            match store.list_entity_ids(tenant.as_str()).await {
                Ok(entities) => {
                    for (entity_type, entity_id) in &entities {
                        self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id);
                    }
                    tracing::info!(
                        tenant = %tenant,
                        count = entities.len(),
                        "hydrated entities from event store"
                    );
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
    pub fn get_or_spawn_tenant_actor_with_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Option<ActorRef<EntityMsg>> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");

        // Check actor registry
        {
            let registry = self.actor_registry.read().unwrap();
            if let Some(actor_ref) = registry.get(&key) {
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

        // Spawn new actor
        let actor = match &self.event_store {
            Some(store) => EntityActor::with_persistence(
                entity_type,
                entity_id,
                table,
                initial_fields,
                store.clone(),
            )
            .with_tenant(tenant.as_str()),
            None => EntityActor::new(entity_type, entity_id, table, initial_fields)
                .with_tenant(tenant.as_str()),
        };
        let actor_ref = self.actor_system.spawn(actor, &key);

        // Register in actor registry
        {
            let mut registry = self.actor_registry.write().unwrap();
            registry.insert(key, actor_ref.clone());
        }

        // Track in entity index for collection queries
        {
            let index_key = format!("{tenant}:{entity_type}");
            let mut index = self.entity_index.write().unwrap();
            index
                .entry(index_key)
                .or_default()
                .insert(entity_id.to_string());
        }

        Some(actor_ref)
    }

    /// Remove an entity from the index and actor registry.
    pub fn remove_entity(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");

        // Remove from actor registry
        {
            let mut registry = self.actor_registry.write().unwrap();
            registry.remove(&actor_key);
        }

        // Remove from entity index
        {
            let index_key = format!("{tenant}:{entity_type}");
            let mut index = self.entity_index.write().unwrap();
            if let Some(ids) = index.get_mut(&index_key) {
                ids.remove(entity_id);
            }
        }
    }

    /// List all entity IDs for a (tenant, entity_type) pair.
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
    /// Accepts `BTreeMap` for DST compliance; converts at the authz boundary.
    pub fn authorize(
        &self,
        headers: &[(String, String)],
        action: &str,
        resource_type: &str,
        resource_attrs: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        let ctx = SecurityContext::from_headers(headers);
        let attrs: std::collections::HashMap<_, _> = resource_attrs // determinism-ok: Cedar API requires HashMap
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(); // determinism-ok
        let decision = self
            .authz
            .authorize_or_bypass(&ctx, action, resource_type, &attrs);
        match decision {
            AuthzDecision::Allow => Ok(()),
            AuthzDecision::Deny(reason) => Err(format!("Authorization denied: {reason}")),
        }
    }

    /// Dispatch an action to an entity actor (legacy single-tenant).
    pub async fn dispatch_action(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        self.dispatch_tenant_action(&TenantId::default(), entity_type, entity_id, action, params)
            .await
    }

    /// Dispatch an action to an entity actor for a specific tenant.
    ///
    /// After a successful action, also triggers any matching reaction rules
    /// for cross-entity coordination.
    pub async fn dispatch_tenant_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        let response = self
            .dispatch_tenant_action_core(tenant, entity_type, entity_id, action, params)
            .await?;

        // Dispatch cross-entity reactions (fire-and-forget, depth 0 = top-level)
        if response.success {
            if let Some(ref dispatcher) = self.reaction_dispatcher {
                let fields = serde_json::to_value(&response.state.fields).unwrap_or_default();
                dispatcher
                    .dispatch_reactions(
                        self,
                        tenant,
                        entity_type,
                        entity_id,
                        action,
                        &response.state.status,
                        &fields,
                        0,
                    )
                    .await;
            }
        }

        Ok(response)
    }

    /// Core dispatch without reaction cascade (used by ReactionDispatcher to
    /// avoid infinite async recursion).
    pub(crate) async fn dispatch_tenant_action_core(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        let Some(actor_ref) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) else {
            // Record a trajectory entry for the "no transition table" failure.
            let entry = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                action: action.to_string(),
                success: false,
                from_status: None,
                to_status: None,
                error: Some(format!(
                    "No transition table for tenant '{tenant}', entity type '{entity_type}'"
                )),
            };
            if let Err(e) = self.persist_trajectory_entry(&entry).await {
                tracing::error!(error = %e, "failed to persist trajectory entry");
            } else if let Ok(mut log) = self.trajectory_log.write() {
                log.push(entry.clone());
            }
            return Err(format!(
                "No transition table for tenant '{tenant}', entity type '{entity_type}'"
            ));
        };

        let response = match actor_ref
            .ask::<EntityResponse>(
                EntityMsg::Action {
                    name: action.to_string(),
                    params,
                },
                Duration::from_secs(5),
            )
            .await
        {
            Ok(response) => response,
            Err(e) => {
                // Record a trajectory entry for actor dispatch failures.
                let entry = TrajectoryEntry {
                    timestamp: sim_now().to_rfc3339(),
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    action: action.to_string(),
                    success: false,
                    from_status: None,
                    to_status: None,
                    error: Some(format!("Actor dispatch failed: {e}")),
                };
                if let Err(persist_err) = self.persist_trajectory_entry(&entry).await {
                    tracing::error!(error = %persist_err, "failed to persist trajectory entry");
                } else if let Ok(mut log) = self.trajectory_log.write() {
                    log.push(entry.clone());
                }
                return Err(format!("Actor dispatch failed: {e}"));
            }
        };

        // Record metrics for the /observe endpoints.
        self.metrics
            .record_transition(entity_type, action, response.success);

        // Record trajectory entry for every completed action (success or failure).
        let trajectory_entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: action.to_string(),
            success: response.success,
            from_status: response.state.events.last().map(|e| e.from_status.clone()),
            to_status: Some(response.state.status.clone()),
            error: if response.success {
                None
            } else {
                Some(
                    response
                        .error
                        .clone()
                        .unwrap_or_else(|| "guard not met".to_string()),
                )
            },
        };
        if let Err(e) = self.persist_trajectory_entry(&trajectory_entry).await {
            tracing::error!(error = %e, "failed to persist trajectory entry");
        } else if let Ok(mut log) = self.trajectory_log.write() {
            log.push(trajectory_entry.clone());
        }

        // Broadcast state change for SSE subscribers (best-effort, ignore send errors)
        if response.success {
            let _ = self.event_tx.send(EntityStateChange {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                action: action.to_string(),
                status: response.state.status.clone(),
                tenant: tenant.to_string(),
            });
            // Update entity state cache for /observe/entities
            let cache_key = format!("{tenant}:{entity_type}:{entity_id}");
            if let Ok(mut cache) = self.entity_state_cache.write() {
                cache.insert(cache_key, (response.state.status.clone(), sim_now()));
            }
        }

        // Fire webhooks (non-blocking — fire-and-forget, failure never affects response)
        if let Some(ref dispatcher) = self.webhook_dispatcher {
            let dispatcher = Arc::clone(dispatcher);
            let entry = trajectory_entry;
            tokio::spawn(async move {
                // determinism-ok: external side-effect, no simulation-visible state
                dispatcher.dispatch(&entry);
            });
        }

        Ok(response)
    }

    /// Get the current state of an entity actor (legacy single-tenant).
    pub async fn get_entity_state(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        self.get_tenant_entity_state(&TenantId::default(), entity_type, entity_id)
            .await
    }

    /// Get the current state of an entity actor for a specific tenant.
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

        actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(5))
            .await
            .map_err(|e| format!("Actor query failed: {e}"))
    }

    /// Create a new entity with initial fields and return its state.
    pub async fn get_or_create_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor_with_fields(tenant, entity_type, entity_id, initial_fields)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let response = actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(5))
            .await
            .map_err(|e| format!("Actor query failed: {e}"))?;

        // Broadcast entity creation event for SSE subscribers
        let _ = self.event_tx.send(EntityStateChange {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: "Created".to_string(),
            status: response.state.status.clone(),
            tenant: tenant.to_string(),
        });

        Ok(response)
    }

    /// Update fields on an existing entity.
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

        actor_ref
            .ask::<EntityResponse>(
                EntityMsg::UpdateFields { fields, replace },
                Duration::from_secs(5),
            )
            .await
            .map_err(|e| format!("Actor update failed: {e}"))
    }

    /// Delete an entity.
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

        let response = actor_ref
            .ask::<EntityResponse>(EntityMsg::Delete, Duration::from_secs(5))
            .await
            .map_err(|e| format!("Actor delete failed: {e}"))?;

        // Stop the actor to release resources
        let _ = actor_ref.stop();

        // Remove from index and registry
        self.remove_entity(tenant, entity_type, entity_id);

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

    /// Update Agent.Hint annotations based on trajectory analysis.
    pub fn enrich_metadata(&self, action_name: &str, hint: &str) {
        self.agent_hints
            .write()
            .unwrap() // ci-ok: infallible lock
            .insert(action_name.to_string(), hint.to_string());
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
            Some(VerificationStatus::Completed(result)) => {
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
