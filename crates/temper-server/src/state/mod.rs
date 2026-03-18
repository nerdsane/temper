//! Server state shared across all request handlers.

mod dispatch;
mod entity_ops;
mod evolution;
pub mod metrics;
pub mod pending_decisions;
mod persistence;
pub mod policy_suggestions;
mod runtime_metrics;
pub mod trajectory;
pub mod wasm_invocation_log;

pub use dispatch::{DispatchCommand, DispatchError, DispatchExtOptions};
pub use entity_ops::{FailedLevelInfo, VerificationGateError};
pub use metrics::MetricsCollector;
pub use pending_decisions::{
    ActionScope, DecisionStatus, DurationScope, PendingDecision, PolicyScopeMatrix, PrincipalScope,
    ResourceScope,
};
pub use policy_suggestions::PolicySuggestionEngine;
pub use trajectory::{TrajectoryEntry, TrajectorySource};
pub use wasm_invocation_log::WasmInvocationEntry;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use temper_authz::AuthzEngine;
use temper_evolution::PostgresRecordStore;
#[allow(deprecated)]
// ADR-0025 Phase 4: remove after sentinel/insight dispatch migrated to IOA entities
use temper_evolution::store::RecordStore;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::scheduler::sim_now;
use temper_spec::csdl::CsdlDocument;
use temper_store_postgres::PostgresEventStore;

use crate::adapters::AdapterRegistry;
use crate::entity_actor::EntityMsg;
use crate::event_store::ServerEventStore;
use crate::events::EntityStateChange;
use crate::idempotency::IdempotencyCache;
use crate::reaction::ReactionDispatcher;
use crate::registry::SpecRegistry;
use crate::secrets::vault::SecretsVault;
use crate::wasm_registry::WasmModuleRegistry;
use crate::webhooks::WebhookDispatcher;
use temper_wasm::WasmEngine;

/// An agent progress event for remote observation via SSE.
///
/// These events are broadcast so that the executor (or any observer) can
/// track agent activity in real time without polling.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentProgressEvent {
    /// Event kind: "tool_call_started", "tool_call_completed",
    /// "task_started", "task_completed", "agent_completed".
    pub kind: String,
    /// The agent ID this event relates to.
    pub agent_id: String,
    /// Optional tool call ID (for tool_call_* events).
    pub tool_call_id: Option<String>,
    /// Optional tool name (for tool_call_* events).
    pub tool_name: Option<String>,
    /// Optional task ID (for task_* events).
    pub task_id: Option<String>,
    /// Optional result or status message.
    pub message: Option<String>,
    /// ISO-8601 timestamp when the event was created.
    pub timestamp: String,
}

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

fn env_bool(name: &str, default: bool) -> bool {
    let val = std::env::var(name); // determinism-ok: read once at startup
    match val {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "no" => false,
            "1" | "true" | "on" | "yes" => true,
            _ => default,
        },
        Err(_) => default,
    }
}

fn env_timeout() -> Duration {
    let secs: u64 = std::env::var("TEMPER_ACTION_TIMEOUT_SECS") // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(5);
    debug_assert!(secs > 0 && secs <= 300, "action timeout must be 1-300s");
    Duration::from_secs(secs)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name) // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn state_cache_budget() -> usize {
    static STATE_CACHE_BUDGET: OnceLock<usize> = OnceLock::new();
    *STATE_CACHE_BUDGET.get_or_init(|| env_usize("TEMPER_STATE_CACHE_BUDGET", 10_000))
}

/// Shared state for the Temper HTTP server.
#[derive(Clone)]
// ADR-0025 Phase 4: remove record_store field after IOA entity migration complete
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
    /// Last access time per actor key (used for idle passivation).
    pub last_accessed: Arc<RwLock<BTreeMap<String, chrono::DateTime<chrono::Utc>>>>,
    /// Optional runtime event store backend for persistence.
    pub event_store: Option<Arc<ServerEventStore>>,
    /// Runtime data directory for persisted local metadata (e.g. specs registry).
    pub data_dir: std::path::PathBuf,
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
    /// In-memory evolution record store (O/P/A/D/I records).
    #[allow(deprecated)] // ADR-0025 Phase 4: remove after chain validation replaced
    pub record_store: Arc<RecordStore>,
    /// Optional Postgres evolution record store (source of truth when configured).
    pub pg_record_store: Option<Arc<PostgresRecordStore>>,
    /// Optional reaction dispatcher for cross-entity coordination.
    ///
    /// Wrapped in `RwLock` so hot-loaded specs can refresh reaction rules at runtime.
    pub reaction_dispatcher: Arc<RwLock<Option<Arc<ReactionDispatcher>>>>,
    /// Optional webhook dispatcher for external system notifications.
    pub webhook_dispatcher: Option<Arc<WebhookDispatcher>>,
    /// Native adapter integration registry (`type = "adapter"` dispatch path).
    pub adapter_registry: Arc<AdapterRegistry>,
    /// WASM module registry: maps (tenant, module_name) → sha256_hash.
    pub wasm_module_registry: Arc<RwLock<WasmModuleRegistry>>,
    /// WASM execution engine: compiles, caches, and invokes sandboxed modules.
    pub wasm_engine: Arc<WasmEngine>,
    /// Global cross-entity invariant enforcement toggle.
    pub cross_invariant_enforce: bool,
    /// Whether eventual invariants should block writes.
    pub cross_invariant_eventual_enforce: bool,
    /// Broadcast channel for design-time events (spec loading, verification progress).
    pub design_time_tx: Arc<tokio::sync::broadcast::Sender<DesignTimeEvent>>,
    /// LRU cache of entity current state, updated on every state change broadcast.
    /// Key: "{tenant}:{entity_type}:{entity_id}", Value: (current_state, last_updated).
    /// Capped at [`state_cache_budget()`] entries; oldest entry evicted automatically.
    #[allow(clippy::type_complexity)]
    pub entity_state_cache:
        Arc<Mutex<lru::LruCache<String, (String, chrono::DateTime<chrono::Utc>)>>>,
    /// Configurable timeout for actor ask operations (default: 5s).
    pub action_dispatch_timeout: Duration,
    /// Eventual invariant convergence tracker.
    pub eventual_tracker: Arc<RwLock<crate::eventual_invariants::EventualInvariantTracker>>,
    /// Idempotency cache for deduplicating agent retries.
    pub idempotency_cache: Arc<IdempotencyCache>,
    /// Optional encrypted secrets vault for per-tenant secret management.
    /// Broadcast channel for new pending decisions (SSE subscriptions).
    pub pending_decision_tx: Arc<tokio::sync::broadcast::Sender<PendingDecision>>,
    /// Per-tenant Cedar policy text (tenant -> policy text).
    pub tenant_policies: Arc<RwLock<BTreeMap<String, String>>>,
    pub secrets_vault: Option<Arc<SecretsVault>>,
    /// Broadcast channel for agent progress events (SSE subscriptions).
    /// // determinism-ok: broadcast channel for external observation only
    pub agent_progress_tx: Arc<tokio::sync::broadcast::Sender<AgentProgressEvent>>,
    /// Listening port for HTTP REPL self-referencing calls.
    pub listen_port: Arc<std::sync::OnceLock<u16>>,
    /// When true, missing `X-Tenant-Id` headers fall back to the first
    /// registered tenant (legacy single-tenant compat).  When false
    /// (multi-tenant mode), a missing header is rejected with 400.
    pub single_tenant_mode: bool,
    /// Denial pattern detection engine for Cedar policy suggestions.
    pub suggestion_engine: Arc<RwLock<PolicySuggestionEngine>>,
    /// When set, spec verification runs in an isolated child process.
    ///
    /// Points to the `temper` binary that supports the `verify-ioa` subcommand.
    /// Each entity's IOA source is written to stdin; the result is read from stdout
    /// as JSON. A 30-second timeout is applied per entity.
    pub verify_subprocess_bin: Option<Arc<std::path::PathBuf>>,
}

#[allow(deprecated)] // ADR-0025 Phase 4: RecordStore used until chain validation replaced
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

        let (event_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (design_time_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (pending_decision_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (agent_progress_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let state = Self {
            actor_system: Arc::new(system),
            csdl: Arc::new(csdl),
            csdl_xml: Arc::new(csdl_xml),
            entity_set_map: Arc::new(entity_set_map),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            last_accessed: Arc::new(RwLock::new(BTreeMap::new())),
            event_store: None,
            data_dir: std::path::PathBuf::new(),
            agent_hints: Arc::new(RwLock::new(BTreeMap::new())),
            authz: Arc::new(AuthzEngine::permissive()),
            registry: Arc::new(RwLock::new(SpecRegistry::new())),
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            event_tx: Arc::new(event_tx),
            start_time: sim_now(),
            metrics: Arc::new(MetricsCollector::new()),
            record_store: Arc::new(RecordStore::new()),
            pg_record_store: None,
            reaction_dispatcher: Arc::new(RwLock::new(None)),
            webhook_dispatcher: None,
            adapter_registry: Arc::new(AdapterRegistry::with_builtins()),
            wasm_module_registry: Arc::new(RwLock::new(WasmModuleRegistry::new())),
            wasm_engine: Arc::new(WasmEngine::default()),
            cross_invariant_enforce: env_bool("TEMPER_XINV_ENFORCE", true),
            cross_invariant_eventual_enforce: env_bool("TEMPER_XINV_EVENTUAL_ENFORCE", true),
            design_time_tx: Arc::new(design_time_tx),
            entity_state_cache: Arc::new(Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(state_cache_budget()).expect("cache budget must be > 0"),
            ))),
            action_dispatch_timeout: env_timeout(),
            eventual_tracker: Arc::new(RwLock::new(
                crate::eventual_invariants::EventualInvariantTracker::new(),
            )),
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            pending_decision_tx: Arc::new(pending_decision_tx),
            tenant_policies: Arc::new(RwLock::new(BTreeMap::new())),
            secrets_vault: None,
            agent_progress_tx: Arc::new(agent_progress_tx), // determinism-ok: broadcast for external observation
            listen_port: Arc::new(std::sync::OnceLock::new()),
            single_tenant_mode: true,
            suggestion_engine: Arc::new(RwLock::new(PolicySuggestionEngine::new())),
            verify_subprocess_bin: None,
        };

        // Pre-register built-in WASM modules (http_fetch for generic HTTP integrations).
        state.register_builtin_wasm_modules();
        state
    }

    /// Compile and register built-in WASM modules (e.g. http_fetch).
    fn register_builtin_wasm_modules(&self) {
        /// Embedded http_fetch WASM binary, compiled from wasm-modules/http-fetch.
        const HTTP_FETCH_WASM: &[u8] =
            include_bytes!("../../../temper-wasm/modules/http_fetch.wasm");

        match self.wasm_engine.compile_and_cache(HTTP_FETCH_WASM) {
            Ok(hash) => {
                if let Ok(mut registry) = self.wasm_module_registry.write() {
                    registry.register_builtin("http_fetch", &hash);
                    tracing::info!(module = "http_fetch", hash = %hash, "registered built-in WASM module");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to compile built-in http_fetch WASM module");
            }
        }
    }

    /// Create ServerState with I/O Automaton TOML specs for transition table resolution.
    ///
    /// Returns an error if any IOA spec fails to parse.
    pub fn with_specs(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut state = Self::new(system, csdl, csdl_xml);
        let mut tables = BTreeMap::new();
        for (entity_type, ioa_source) in &ioa_sources {
            let table = TransitionTable::try_from_ioa_source(ioa_source)
                .map_err(|e| format!("entity '{entity_type}': {e}"))?;
            tables.insert(entity_type.clone(), Arc::new(table));
        }
        state.transition_tables = Arc::new(tables);
        Ok(state)
    }

    /// Create ServerState with specs AND Postgres persistence.
    ///
    /// Returns an error if any IOA spec fails to parse.
    pub fn with_persistence(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
        store: PostgresEventStore,
    ) -> Result<Self, String> {
        let mut state = Self::with_specs(system, csdl, csdl_xml, ioa_sources)?;
        state.event_store = Some(Arc::new(ServerEventStore::Postgres(store)));
        Ok(state)
    }

    /// Create ServerState with specs and an explicit runtime event store.
    ///
    /// Returns an error if any IOA spec fails to parse.
    pub fn with_event_store(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
        store: ServerEventStore,
    ) -> Result<Self, String> {
        let mut state = Self::with_specs(system, csdl, csdl_xml, ioa_sources)?;
        state.event_store = Some(Arc::new(store));
        Ok(state)
    }

    /// Create ServerState from a [`SpecRegistry`] in single-tenant compatibility mode.
    ///
    /// Used by tests and simple setups.  For multi-tenant production use
    /// [`from_registry_shared`](Self::from_registry_shared) instead.
    pub fn from_registry(system: ActorSystem, registry: SpecRegistry) -> Self {
        let mut state = Self::from_registry_shared(system, Arc::new(RwLock::new(registry)));
        state.single_tenant_mode = true;
        state
    }

    /// Create ServerState from a shared, mutable [`SpecRegistry`].
    ///
    /// Use this when the registry must be shared with another component
    /// (e.g. `PlatformState`) so that writes are visible to dispatch.
    pub fn from_registry_shared(system: ActorSystem, registry: Arc<RwLock<SpecRegistry>>) -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (design_time_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (pending_decision_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (agent_progress_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let state = Self {
            actor_system: Arc::new(system),
            csdl: Arc::new(CsdlDocument {
                version: "4.0".into(),
                schemas: vec![],
            }),
            csdl_xml: Arc::new(String::new()),
            entity_set_map: Arc::new(BTreeMap::new()),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            last_accessed: Arc::new(RwLock::new(BTreeMap::new())),
            event_store: None,
            data_dir: std::path::PathBuf::new(),
            agent_hints: Arc::new(RwLock::new(BTreeMap::new())),
            authz: Arc::new(AuthzEngine::permissive()),
            registry,
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            event_tx: Arc::new(event_tx),
            start_time: sim_now(),
            metrics: Arc::new(MetricsCollector::new()),
            record_store: Arc::new(RecordStore::new()),
            pg_record_store: None,
            reaction_dispatcher: Arc::new(RwLock::new(None)),
            webhook_dispatcher: None,
            adapter_registry: Arc::new(AdapterRegistry::with_builtins()),
            wasm_module_registry: Arc::new(RwLock::new(WasmModuleRegistry::new())),
            wasm_engine: Arc::new(WasmEngine::default()),
            cross_invariant_enforce: env_bool("TEMPER_XINV_ENFORCE", true),
            cross_invariant_eventual_enforce: env_bool("TEMPER_XINV_EVENTUAL_ENFORCE", true),
            design_time_tx: Arc::new(design_time_tx),
            entity_state_cache: Arc::new(Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(state_cache_budget()).expect("cache budget must be > 0"),
            ))),
            action_dispatch_timeout: env_timeout(),
            eventual_tracker: Arc::new(RwLock::new(
                crate::eventual_invariants::EventualInvariantTracker::new(),
            )),
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            pending_decision_tx: Arc::new(pending_decision_tx),
            tenant_policies: Arc::new(RwLock::new(BTreeMap::new())),
            secrets_vault: None,
            agent_progress_tx: Arc::new(agent_progress_tx), // determinism-ok: broadcast for external observation
            listen_port: Arc::new(std::sync::OnceLock::new()),
            single_tenant_mode: false,
            suggestion_engine: Arc::new(RwLock::new(PolicySuggestionEngine::new())),
            verify_subprocess_bin: None,
        };
        state.register_builtin_wasm_modules();
        state
    }

    /// Attach a reaction dispatcher for cross-entity coordination.
    pub fn with_reaction_dispatcher(self, dispatcher: Arc<ReactionDispatcher>) -> Self {
        if let Ok(mut slot) = self.reaction_dispatcher.write() {
            *slot = Some(dispatcher);
        }
        self
    }

    /// Rebuild and install reaction dispatcher from the current spec registry.
    pub fn rebuild_reaction_dispatcher(&self) {
        let reaction_registry = {
            let registry = self.registry.read().unwrap();
            registry.build_reaction_registry()
        };
        let dispatcher = Arc::new(ReactionDispatcher::new(Arc::new(reaction_registry)));
        if let Ok(mut slot) = self.reaction_dispatcher.write() {
            *slot = Some(dispatcher);
        }
    }

    /// Attach a webhook dispatcher for external system notifications.
    pub fn with_webhook_dispatcher(mut self, dispatcher: Arc<WebhookDispatcher>) -> Self {
        self.webhook_dispatcher = Some(dispatcher);
        self
    }

    /// Override cross-invariant enforcement mode.
    pub fn with_cross_invariant_enforcement(
        mut self,
        enforce: bool,
        eventual_enforce: bool,
    ) -> Self {
        self.cross_invariant_enforce = enforce;
        self.cross_invariant_eventual_enforce = eventual_enforce;
        self
    }

    /// Attach a Postgres-backed evolution record store.
    pub fn with_pg_record_store(mut self, store: PostgresRecordStore) -> Self {
        self.pg_record_store = Some(Arc::new(store));
        self
    }

    /// Attach an encrypted secrets vault.
    pub fn with_secrets_vault(mut self, vault: SecretsVault) -> Self {
        self.secrets_vault = Some(Arc::new(vault));
        self
    }

    /// Insert/update an entity status cache entry.
    ///
    /// The underlying [`lru::LruCache`] automatically evicts the least-recently-used
    /// entry when the budget (see [`state_cache_budget`]) is exceeded, so no manual
    /// eviction loop is needed here.
    pub fn cache_entity_status(&self, cache_key: String, status: String) {
        if let Ok(mut cache) = self.entity_state_cache.lock() {
            cache.put(cache_key, (status, sim_now()));
        }
    }

    /// Get a reference to the platform Turso event store.
    ///
    /// Panics if the event store is not configured or is not a Turso backend.
    pub fn turso(&self) -> &temper_store_turso::TursoEventStore {
        self.platform_persistent_store()
            .expect("Turso event store is not configured")
    }

    /// Get an optional reference to the **platform** Turso event store.
    ///
    /// Only use for system-wide data that stays in the platform DB
    /// (evolution records, feature requests, tenant registry).
    /// For tenant-scoped data, use [`persistent_store_for_tenant`].
    ///
    /// Returns `None` when the server is running without Turso (e.g. in-memory
    /// mode or tests). Callers should degrade gracefully to empty results.
    pub fn platform_persistent_store(&self) -> Option<&temper_store_turso::TursoEventStore> {
        self.event_store
            .as_ref()
            .and_then(|store| store.platform_turso_store())
    }

    /// Get a Turso store for a specific tenant.
    ///
    /// In TenantRouted mode, routes to the per-tenant database.
    /// In single-DB Turso mode, returns the shared store.
    /// `temper-system` and `default` tenants route to the platform store.
    ///
    /// Returns `None` when the server is running without Turso.
    pub async fn persistent_store_for_tenant(
        &self,
        tenant: &str,
    ) -> Option<temper_store_turso::TursoEventStore> {
        let store = self.event_store.as_ref()?;
        store.turso_for_tenant(tenant).await
    }

    /// Find an entity spec by name across all tenants.
    ///
    /// Returns the owning tenant and the IOA source string on success.
    /// Acquires a read lock on the spec registry.
    pub fn find_entity_ioa_source(
        &self,
        entity: &str,
    ) -> Option<(temper_runtime::tenant::TenantId, String)> {
        let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
        for tenant_id in registry.tenant_ids() {
            if let Some(entity_spec) = registry.get_spec(tenant_id, entity) {
                return Some((tenant_id.clone(), entity_spec.ioa_source.clone()));
            }
        }
        None
    }

    /// Load aggregated unmet-intent failure groups from Turso.
    ///
    /// Uses fan-out across all tenant stores in TenantRouted mode.
    /// Returns an empty vec when Turso is not configured.
    pub async fn load_unmet_intent_rows_aggregated(
        &self,
    ) -> (
        Vec<temper_store_turso::UnmetIntentAggRow>,
        std::collections::BTreeMap<String, String>,
    ) {
        let stores = self.collect_all_turso_stores().await;
        if stores.is_empty() {
            return (Vec::new(), std::collections::BTreeMap::new());
        }

        let mut failures = Vec::new();
        let mut submitted_specs = std::collections::BTreeMap::new();

        for turso in &stores {
            match turso.load_unmet_intent_rows().await {
                Ok(rows) => failures.extend(rows),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load unmet intent rows from Turso");
                }
            }
            match turso.load_submit_spec_timestamps().await {
                Ok(map) => submitted_specs.extend(map),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load submit-spec timestamps from Turso");
                }
            }
        }
        (failures, submitted_specs)
    }

    /// Count trajectory rows per tenant using fan-out across all stores.
    ///
    /// Returns an empty map when Turso is not configured.
    pub async fn count_trajectories_by_tenant(&self) -> std::collections::BTreeMap<String, u64> {
        let stores = self.collect_all_turso_stores().await;
        if stores.is_empty() {
            return std::collections::BTreeMap::new();
        }

        let mut counts = std::collections::BTreeMap::new();
        for turso in &stores {
            match turso.count_trajectories_by_tenant().await {
                Ok(c) => {
                    for (tenant, count) in c {
                        *counts.entry(tenant).or_insert(0) += count;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to count trajectories by tenant from Turso");
                }
            }
        }
        counts
    }

    /// Load trajectory entries from Turso, converting to domain TrajectoryEntry.
    ///
    /// Uses fan-out across all tenant stores in TenantRouted mode.
    pub async fn load_trajectory_entries(&self, limit: i64) -> Vec<TrajectoryEntry> {
        let stores = self.collect_all_turso_stores().await;
        if stores.is_empty() {
            return Vec::new();
        }

        let mut all_entries = Vec::new();
        for turso in &stores {
            match turso.load_recent_trajectories(limit).await {
                Ok(rows) => {
                    all_entries.extend(rows.into_iter().map(|r| TrajectoryEntry {
                        timestamp: r.created_at,
                        tenant: r.tenant,
                        entity_type: r.entity_type,
                        entity_id: r.entity_id,
                        action: r.action,
                        success: r.success,
                        from_status: r.from_status,
                        to_status: r.to_status,
                        error: r.error,
                        agent_id: r.agent_id,
                        session_id: r.session_id,
                        authz_denied: r.authz_denied,
                        denied_resource: r.denied_resource,
                        denied_module: r.denied_module,
                        source: r.source.as_deref().and_then(|s| match s {
                            "Entity" => Some(TrajectorySource::Entity),
                            "Platform" => Some(TrajectorySource::Platform),
                            "Authz" => Some(TrajectorySource::Authz),
                            _ => None,
                        }),
                        spec_governed: r.spec_governed,
                        agent_type: None,
                        request_body: r.request_body.and_then(|s| serde_json::from_str(&s).ok()),
                        intent: r.intent,
                    }));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load trajectories from Turso");
                }
            }
        }
        // Sort by timestamp descending and limit
        all_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        all_entries.truncate(limit as usize);
        all_entries
    }

    /// Collect all Turso stores for fan-out reads.
    ///
    /// In single-DB mode, returns just the shared store.
    /// In TenantRouted mode, returns the platform store + all connected tenant stores.
    /// Returns an empty vec when Turso is not configured.
    pub async fn collect_all_turso_stores(&self) -> Vec<temper_store_turso::TursoEventStore> {
        let Some(store) = self.event_store.as_ref() else {
            return Vec::new();
        };

        if let Some(turso) = store.turso_store() {
            return vec![turso.clone()];
        }

        if let Some(router) = store.tenant_router() {
            let mut stores = vec![router.platform_store().clone()];
            for tid in router.connected_tenants().await {
                if let Ok(s) = router.store_for_tenant(&tid).await {
                    stores.push(s);
                }
            }
            return stores;
        }

        Vec::new()
    }
}
