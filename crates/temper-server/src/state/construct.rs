//! ServerState constructors and builder methods.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, RwLock};

use temper_authz::AuthzEngine;
#[allow(deprecated)]
use temper_evolution::store::RecordStore;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::sim_now;
use temper_spec::automaton::parse_automaton;
use temper_spec::csdl::CsdlDocument;
use temper_store_postgres::PostgresEventStore;

use crate::adapters::AdapterRegistry;
use crate::idempotency::IdempotencyCache;
use crate::internal_invocation::InternalInvocationCredentialStore;
use crate::registry::SpecRegistry;
use crate::storage::StorageStack;
use crate::trigger::ReactionDispatcher;
use crate::wasm_registry::WasmModuleRegistry;
use temper_wasm::WasmEngine;

use super::{
    AdmissionController, AgentProgressEvent, EntityObserveEvent, MetricsCollector,
    PolicySuggestionEngine, ServerState, StateTimeoutTracker, env_bool, env_local_tdata_hosts,
    env_timeout, install_liveness_metrics_reporter_once, state_cache_budget,
};

impl ServerState {
    /// Create ServerState from CSDL XML and optional specification sources.
    pub fn new(system: ActorSystem, csdl: CsdlDocument, csdl_xml: String) -> Self {
        install_liveness_metrics_reporter_once();
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
        let (entity_observe_tx, _) = tokio::sync::broadcast::channel(512); // determinism-ok: broadcast for external observation
        let (design_time_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (pending_decision_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (agent_progress_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (observe_refresh_tx, _) = tokio::sync::broadcast::channel(64); // determinism-ok: broadcast for external observation
        let state = Self {
            capture_health: crate::trajectory_outbox::CaptureHealth::default(),
            actor_system: Arc::new(system),
            pg_actor_system: None,
            actor_backed_types: BTreeSet::new(),
            csdl: Arc::new(csdl),
            csdl_xml: Arc::new(csdl_xml),
            entity_set_map: Arc::new(entity_set_map),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            last_accessed: Arc::new(RwLock::new(BTreeMap::new())),
            storage_stack: None,
            query_projection_queue: Arc::new(Mutex::new(None)),
            snapshot_write_queue: Arc::new(Mutex::new(None)),
            ots_trajectory_outbox: Arc::new(Mutex::new(None)),
            data_dir: std::path::PathBuf::new(),
            agent_hints: Arc::new(RwLock::new(BTreeMap::new())),
            // Network and tenant-scoped authorization starts fail-closed.
            // Tests/development that intentionally need a permissive tenant
            // must install that tenant policy explicitly (ARN-230).
            authz: Arc::new(AuthzEngine::empty()),
            registry: Arc::new(RwLock::new(SpecRegistry::new())),
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            entity_index_hydrated: Arc::new(RwLock::new(BTreeSet::new())),
            key_index_backfilled: Arc::new(RwLock::new(BTreeMap::new())),
            key_index_watermarks_loaded: Arc::new(RwLock::new(BTreeSet::new())),
            event_tx: Arc::new(event_tx),
            entity_observe_tx: Arc::new(entity_observe_tx),
            start_time: sim_now(),
            metrics: Arc::new(MetricsCollector::new()),
            #[allow(deprecated)]
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
            admission: Arc::new(AdmissionController::new()),
            state_timeout_tracker: Arc::new(StateTimeoutTracker::new()),
            eventual_tracker: Arc::new(RwLock::new(
                crate::eventual_invariants::EventualInvariantTracker::new(),
            )),
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            internal_invocation_credentials: InternalInvocationCredentialStore::runtime(),
            pending_decision_tx: Arc::new(pending_decision_tx),
            tenant_policies: Arc::new(RwLock::new(BTreeMap::new())),
            #[cfg(feature = "observe")]
            policy_approval_lock: Arc::new(tokio::sync::Mutex::new(())),
            commons_guardrail_tenants: Arc::new(RwLock::new(BTreeSet::new())),
            commons_rate_limit_buckets: Arc::new(Mutex::new(BTreeMap::new())),
            commons_storage_projection_cache: Arc::new(Mutex::new(BTreeMap::new())),
            commons_storage_reservations: Arc::new(Mutex::new(BTreeMap::new())),
            commons_write_guardrail_lock: Arc::new(tokio::sync::Mutex::new(())),
            raw_blob_ingest_budget: crate::blob_store::BlobIngestBudget::runtime(),
            secrets_vault: None,
            agent_progress_tx: Arc::new(agent_progress_tx), // determinism-ok: broadcast for external observation
            entity_event_sequences: Arc::new(Mutex::new(BTreeMap::new())),
            entity_observe_log: Arc::new(Mutex::new(BTreeMap::new())),
            observe_refresh_tx: Arc::new(observe_refresh_tx), // determinism-ok: broadcast for external observation
            listen_port: Arc::new(std::sync::OnceLock::new()),
            single_tenant_mode: true,
            suggestion_engine: Arc::new(RwLock::new(PolicySuggestionEngine::new())),
            verify_subprocess_bin: None,
            custom_effect_handler: None,
            bound_action_hook: None,
            http_endpoint_tables: Arc::new(crate::http_endpoint::HttpEndpointTables::new()),
            http_stream_registry: Arc::new(temper_wasm::http_stream::HttpStreamRegistry::new()),
            workflow_spans: Arc::new(crate::workflow_tracing::WorkflowSpanRegistry::default()),
            local_tdata_hosts: Arc::new(env_local_tdata_hosts()),
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

    /// Append one observe event to the per-entity replay buffer and broadcast it.
    fn push_entity_observe_event(&self, event: EntityObserveEvent) {
        let key = format!("{}:{}:{}", event.tenant, event.entity_type, event.entity_id);
        {
            let mut log = self.entity_observe_log.lock().unwrap(); // ci-ok: infallible lock
            let entries = log.entry(key).or_default();
            entries.push(event.clone());
            if entries.len() > 512 {
                let overflow = entries.len().saturating_sub(512);
                entries.drain(0..overflow);
            }
        }
        let _ = self.entity_observe_tx.send(event);
    }

    /// Allocate the next observe-event sequence number for an entity.
    pub(crate) fn next_entity_event_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> u64 {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let mut sequences = self.entity_event_sequences.lock().unwrap(); // ci-ok: infallible lock
        let next = sequences.get(&key).copied().unwrap_or(0) + 1;
        sequences.insert(key, next);
        next
    }

    /// Record an observe event using a caller-supplied sequence number.
    pub(crate) fn record_entity_observe_event_with_seq(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        seq: u64,
        event_name: &str,
        data: serde_json::Value,
    ) {
        let event = EntityObserveEvent {
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            seq,
            event_name: event_name.to_string(),
            data,
        };
        self.push_entity_observe_event(event);
    }

    #[cfg(feature = "observe")]
    /// Replay buffered observe events with `seq` greater than `since`.
    pub(crate) fn replay_entity_observe_events(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        since: u64,
    ) -> Vec<EntityObserveEvent> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let log = self.entity_observe_log.lock().unwrap(); // ci-ok: infallible lock
        log.get(&key)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|event| event.seq > since)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Broadcast an agent-progress event and mirror it into the observe log.
    pub(crate) fn broadcast_agent_progress(&self, event: AgentProgressEvent) {
        let _ = self.agent_progress_tx.send(event.clone());
        let observe_event = EntityObserveEvent {
            tenant: event.tenant.clone(),
            entity_type: event.entity_type.clone(),
            entity_id: event.entity_id.clone(),
            seq: event.seq,
            event_name: event.kind.clone(),
            data: serde_json::to_value(&event).unwrap_or_default(),
        };
        self.push_entity_observe_event(observe_event);
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
            let automaton = parse_automaton(ioa_source).map_err(|e| {
                format!("entity '{entity_type}': failed to parse I/O Automaton TOML: {e}")
            })?;
            let table = TransitionTable::from_automaton(&automaton);
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
        state.set_storage_stack(StorageStack::from_postgres(store));
        Ok(state)
    }

    /// Create ServerState with specs and an explicit storage stack.
    ///
    /// Returns an error if any IOA spec fails to parse.
    pub fn with_storage_stack(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
        stack: StorageStack,
    ) -> Result<Self, String> {
        let mut state = Self::with_specs(system, csdl, csdl_xml, ioa_sources)?;
        state.set_storage_stack(stack);
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
        let (entity_observe_tx, _) = tokio::sync::broadcast::channel(512); // determinism-ok: broadcast for external observation
        let (design_time_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (pending_decision_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (agent_progress_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (observe_refresh_tx, _) = tokio::sync::broadcast::channel(64); // determinism-ok: broadcast for external observation
        let state = Self {
            capture_health: crate::trajectory_outbox::CaptureHealth::default(),
            actor_system: Arc::new(system),
            pg_actor_system: None,
            actor_backed_types: BTreeSet::new(),
            csdl: Arc::new(CsdlDocument {
                version: "4.0".into(),
                schemas: vec![],
            }),
            csdl_xml: Arc::new(String::new()),
            entity_set_map: Arc::new(BTreeMap::new()),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            last_accessed: Arc::new(RwLock::new(BTreeMap::new())),
            storage_stack: None,
            query_projection_queue: Arc::new(Mutex::new(None)),
            snapshot_write_queue: Arc::new(Mutex::new(None)),
            ots_trajectory_outbox: Arc::new(Mutex::new(None)),
            data_dir: std::path::PathBuf::new(),
            agent_hints: Arc::new(RwLock::new(BTreeMap::new())),
            // Missing tenant policy state is never an implicit permit-all
            // compatibility mode (ARN-230).
            authz: Arc::new(AuthzEngine::empty()),
            registry,
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            entity_index_hydrated: Arc::new(RwLock::new(BTreeSet::new())),
            key_index_backfilled: Arc::new(RwLock::new(BTreeMap::new())),
            key_index_watermarks_loaded: Arc::new(RwLock::new(BTreeSet::new())),
            event_tx: Arc::new(event_tx),
            entity_observe_tx: Arc::new(entity_observe_tx),
            start_time: sim_now(),
            metrics: Arc::new(MetricsCollector::new()),
            #[allow(deprecated)]
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
            admission: Arc::new(AdmissionController::new()),
            state_timeout_tracker: Arc::new(StateTimeoutTracker::new()),
            eventual_tracker: Arc::new(RwLock::new(
                crate::eventual_invariants::EventualInvariantTracker::new(),
            )),
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            internal_invocation_credentials: InternalInvocationCredentialStore::runtime(),
            pending_decision_tx: Arc::new(pending_decision_tx),
            tenant_policies: Arc::new(RwLock::new(BTreeMap::new())),
            #[cfg(feature = "observe")]
            policy_approval_lock: Arc::new(tokio::sync::Mutex::new(())),
            commons_guardrail_tenants: Arc::new(RwLock::new(BTreeSet::new())),
            commons_rate_limit_buckets: Arc::new(Mutex::new(BTreeMap::new())),
            commons_storage_projection_cache: Arc::new(Mutex::new(BTreeMap::new())),
            commons_storage_reservations: Arc::new(Mutex::new(BTreeMap::new())),
            commons_write_guardrail_lock: Arc::new(tokio::sync::Mutex::new(())),
            raw_blob_ingest_budget: crate::blob_store::BlobIngestBudget::runtime(),
            secrets_vault: None,
            agent_progress_tx: Arc::new(agent_progress_tx), // determinism-ok: broadcast for external observation
            entity_event_sequences: Arc::new(Mutex::new(BTreeMap::new())),
            entity_observe_log: Arc::new(Mutex::new(BTreeMap::new())),
            observe_refresh_tx: Arc::new(observe_refresh_tx), // determinism-ok: broadcast for external observation
            listen_port: Arc::new(std::sync::OnceLock::new()),
            single_tenant_mode: false,
            suggestion_engine: Arc::new(RwLock::new(PolicySuggestionEngine::new())),
            verify_subprocess_bin: None,
            custom_effect_handler: None,
            bound_action_hook: None,
            http_endpoint_tables: Arc::new(crate::http_endpoint::HttpEndpointTables::new()),
            http_stream_registry: Arc::new(temper_wasm::http_stream::HttpStreamRegistry::new()),
            workflow_spans: Arc::new(crate::workflow_tracing::WorkflowSpanRegistry::default()),
            local_tdata_hosts: Arc::new(env_local_tdata_hosts()),
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
}
