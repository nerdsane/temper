//! Server state shared across all request handlers.

mod dispatch;
mod dispatch_blocking;
mod entity_ops;
pub mod metrics;
pub mod pending_decisions;
mod persistence;
pub mod trajectory;
pub mod wasm_invocation_log;

pub use entity_ops::{FailedLevelInfo, VerificationGateError};
pub use metrics::MetricsCollector;
pub use pending_decisions::{DecisionStatus, PendingDecision, PendingDecisionLog, PolicyScope};
pub use trajectory::{TrajectoryEntry, TrajectoryLog};
pub use wasm_invocation_log::{WasmInvocationEntry, WasmInvocationLog};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use temper_authz::AuthzEngine;
use temper_evolution::{PostgresRecordStore, RecordStore};
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::scheduler::sim_now;
use temper_spec::csdl::CsdlDocument;
use temper_store_postgres::PostgresEventStore;

use crate::entity_actor::EntityMsg;
use crate::event_store::ServerEventStore;
use crate::events::EntityStateChange;
use crate::idempotency::IdempotencyCache;
use crate::reaction::ReactionDispatcher;
use crate::registry::SpecRegistry;
use crate::secrets_vault::SecretsVault;
use crate::wasm_registry::WasmModuleRegistry;
use crate::webhooks::WebhookDispatcher;
use temper_wasm::WasmEngine;

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

/// Maximum number of trajectory entries retained in the bounded log.
pub(crate) const TRAJECTORY_LOG_CAPACITY: usize = 10_000;
/// Maximum number of design-time events retained in memory.
const DESIGN_TIME_LOG_CAPACITY: usize = 10_000;
/// Maximum number of WASM invocation entries retained in the bounded log.
const WASM_INVOCATION_LOG_CAPACITY: usize = 500;

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        // determinism-ok: read once at startup, not per simulation step
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
    /// Bounded trajectory log for failed intent analysis and Evolution Engine.
    pub trajectory_log: Arc<RwLock<TrajectoryLog>>,
    /// In-memory evolution record store (O/P/A/D/I records).
    pub record_store: Arc<RecordStore>,
    /// Optional Postgres evolution record store (source of truth when configured).
    pub pg_record_store: Option<Arc<PostgresRecordStore>>,
    /// Optional reaction dispatcher for cross-entity coordination.
    ///
    /// Wrapped in `RwLock` so hot-loaded specs can refresh reaction rules at runtime.
    pub reaction_dispatcher: Arc<RwLock<Option<Arc<ReactionDispatcher>>>>,
    /// Optional webhook dispatcher for external system notifications.
    pub webhook_dispatcher: Option<Arc<WebhookDispatcher>>,
    /// WASM module registry: maps (tenant, module_name) → sha256_hash.
    pub wasm_module_registry: Arc<RwLock<WasmModuleRegistry>>,
    /// WASM execution engine: compiles, caches, and invokes sandboxed modules.
    pub wasm_engine: Arc<WasmEngine>,
    /// Bounded WASM invocation log for observability.
    pub wasm_invocation_log: Arc<RwLock<WasmInvocationLog>>,
    /// Global cross-entity invariant enforcement toggle.
    pub cross_invariant_enforce: bool,
    /// Whether eventual invariants should block writes.
    pub cross_invariant_eventual_enforce: bool,
    /// Broadcast channel for design-time events (spec loading, verification progress).
    pub design_time_tx: Arc<tokio::sync::broadcast::Sender<DesignTimeEvent>>,
    /// In-memory log of design-time events for workflow history (append-only, bounded).
    pub design_time_log: Arc<RwLock<Vec<DesignTimeEvent>>>,
    /// Cache of entity current state, updated on every state change broadcast.
    /// Key: "{tenant}:{entity_type}:{entity_id}", Value: (current_state, last_updated).
    #[allow(clippy::type_complexity)]
    pub entity_state_cache: Arc<RwLock<BTreeMap<String, (String, chrono::DateTime<chrono::Utc>)>>>,
    /// Configurable timeout for actor ask operations (default: 5s).
    pub action_dispatch_timeout: Duration,
    /// Eventual invariant convergence tracker.
    pub eventual_tracker: Arc<RwLock<crate::eventual_invariants::EventualInvariantTracker>>,
    /// Idempotency cache for deduplicating agent retries.
    pub idempotency_cache: Arc<IdempotencyCache>,
    /// Optional encrypted secrets vault for per-tenant secret management.
    /// Bounded log of pending authorization decisions awaiting human review.
    pub pending_decision_log: Arc<RwLock<PendingDecisionLog>>,
    /// Broadcast channel for new pending decisions (SSE subscriptions).
    pub pending_decision_tx: Arc<tokio::sync::broadcast::Sender<PendingDecision>>,
    /// Per-tenant Cedar policy text (tenant -> policy text).
    pub tenant_policies: Arc<RwLock<BTreeMap<String, String>>>,
    pub secrets_vault: Option<Arc<SecretsVault>>,
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
        let (pending_decision_tx, _) = tokio::sync::broadcast::channel(256);
        let state = Self {
            actor_system: Arc::new(system),
            csdl: Arc::new(csdl),
            csdl_xml: Arc::new(csdl_xml),
            entity_set_map: Arc::new(entity_set_map),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            event_store: None,
            data_dir: std::path::PathBuf::new(),
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
            reaction_dispatcher: Arc::new(RwLock::new(None)),
            webhook_dispatcher: None,
            wasm_module_registry: Arc::new(RwLock::new(WasmModuleRegistry::new())),
            wasm_engine: Arc::new(WasmEngine::default()),
            wasm_invocation_log: Arc::new(RwLock::new(WasmInvocationLog::new(
                WASM_INVOCATION_LOG_CAPACITY,
            ))),
            cross_invariant_enforce: env_bool("TEMPER_XINV_ENFORCE", true),
            cross_invariant_eventual_enforce: env_bool("TEMPER_XINV_EVENTUAL_ENFORCE", true),
            design_time_tx: Arc::new(design_time_tx),
            design_time_log: Arc::new(RwLock::new(Vec::new())),
            entity_state_cache: Arc::new(RwLock::new(BTreeMap::new())),
            action_dispatch_timeout: env_timeout(),
            eventual_tracker: Arc::new(RwLock::new(
                crate::eventual_invariants::EventualInvariantTracker::new(),
            )),
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            pending_decision_log: Arc::new(RwLock::new(PendingDecisionLog::new())),
            pending_decision_tx: Arc::new(pending_decision_tx),
            tenant_policies: Arc::new(RwLock::new(BTreeMap::new())),
            secrets_vault: None,
        };

        // Pre-register built-in WASM modules (http_fetch for generic HTTP integrations).
        state.register_builtin_wasm_modules();
        state
    }

    /// Compile and register built-in WASM modules (e.g. http_fetch).
    fn register_builtin_wasm_modules(&self) {
        /// Embedded http_fetch WASM binary, compiled from examples/wasm-modules/http-fetch.
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
        let (pending_decision_tx, _) = tokio::sync::broadcast::channel(256);
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
            event_store: None,
            data_dir: std::path::PathBuf::new(),
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
            reaction_dispatcher: Arc::new(RwLock::new(None)),
            webhook_dispatcher: None,
            wasm_module_registry: Arc::new(RwLock::new(WasmModuleRegistry::new())),
            wasm_engine: Arc::new(WasmEngine::default()),
            wasm_invocation_log: Arc::new(RwLock::new(WasmInvocationLog::new(
                WASM_INVOCATION_LOG_CAPACITY,
            ))),
            cross_invariant_enforce: env_bool("TEMPER_XINV_ENFORCE", true),
            cross_invariant_eventual_enforce: env_bool("TEMPER_XINV_EVENTUAL_ENFORCE", true),
            design_time_tx: Arc::new(design_time_tx),
            design_time_log: Arc::new(RwLock::new(Vec::new())),
            entity_state_cache: Arc::new(RwLock::new(BTreeMap::new())),
            action_dispatch_timeout: env_timeout(),
            eventual_tracker: Arc::new(RwLock::new(
                crate::eventual_invariants::EventualInvariantTracker::new(),
            )),
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            pending_decision_log: Arc::new(RwLock::new(PendingDecisionLog::new())),
            pending_decision_tx: Arc::new(pending_decision_tx),
            tenant_policies: Arc::new(RwLock::new(BTreeMap::new())),
            secrets_vault: None,
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
}
