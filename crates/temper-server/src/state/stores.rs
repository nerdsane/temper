//! Storage and query-plane accessors on ServerState.

use std::sync::{Arc, RwLock};

use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{StreamRegistry, WasmInvocationContext};

use crate::entity_actor::SnapshotWriteQueue;
use crate::ots_trajectory_outbox::OtsTrajectoryOutbox;
use crate::registry::SpecRegistry;
use crate::secrets::vault::SecretsVault;
use crate::storage::{
    BackendLabel, BoxedEventStore, DataOnlyCreateStore, MetadataStore, PolicyStore,
    QueryPlaneStore, StorageStack, TrajectorySink,
};
use crate::webhooks::WebhookDispatcher;
use temper_actor_runtime::ActorSystem as PgActorSystem;
use temper_evolution::PostgresRecordStore;
use temper_runtime::ActorSystem;

use super::{IndexedFileStreamRead, QueryProjectionWriteQueue, ServerState};

impl ServerState {
    /// Enable commons-mode write guardrails for a tenant.
    pub fn enable_commons_guardrails(&self, tenant: &str) {
        if let Ok(mut tenants) = self.commons_guardrail_tenants.write() {
            tenants.insert(tenant.to_string());
        }
    }

    /// Whether commons-mode write guardrails are active for a tenant.
    pub fn commons_guardrails_enabled(&self, tenant: &TenantId) -> bool {
        self.commons_guardrail_tenants
            .read()
            .map(|tenants| tenants.contains(tenant.as_str()))
            .unwrap_or(false)
    }

    /// Attach the composed runtime storage stack.
    pub fn set_storage_stack(&mut self, stack: StorageStack) {
        let stack = Arc::new(stack);
        let snapshot_queue = SnapshotWriteQueue::start(stack.events.clone());
        if let Ok(mut slot) = self.snapshot_write_queue.lock() {
            *slot = Some(snapshot_queue);
        }
        if let Some(query_plane) = stack.query_plane.clone() {
            let queue = QueryProjectionWriteQueue::start(query_plane);
            if let Ok(mut slot) = self.query_projection_queue.lock() {
                *slot = Some(queue);
            }
        }
        if let Some(metadata) = stack.metadata.clone() {
            let queue = OtsTrajectoryOutbox::start();
            queue.recover_queued_metadata_stores(stack.backend.as_str(), metadata);
            if let Ok(mut slot) = self.ots_trajectory_outbox.lock() {
                *slot = Some(queue);
            }
        }
        self.storage_stack = Some(stack);
    }

    /// Return the durable query-plane capability for projection reads/writes.
    pub(crate) fn query_plane_store(&self) -> Option<Arc<dyn QueryPlaneStore>> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.query_plane.clone())
    }

    /// Return the native data-only create capability when the backend supports it.
    pub(crate) fn data_only_create_store(&self) -> Option<Arc<dyn DataOnlyCreateStore>> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.data_only_create.clone())
    }

    /// Return the runtime event journal capability plus backend label.
    pub(crate) fn event_journal(&self) -> Option<(BoxedEventStore, BackendLabel)> {
        self.storage_stack
            .as_ref()
            .map(|stack| (stack.events.clone(), stack.backend))
    }

    /// Return the granular Cedar policy persistence capability.
    pub fn policy_store(&self) -> Option<Arc<dyn PolicyStore>> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.policies.clone())
    }

    /// Return the observe trajectory sink plus backend label for metrics.
    pub(crate) fn trajectory_sink(&self) -> Option<(&'static str, Arc<dyn TrajectorySink>)> {
        if let Some(stack) = self.storage_stack.as_ref()
            && let Some(sink) = stack.trajectory.clone()
        {
            return Some((stack.backend.as_str(), sink));
        }

        None
    }

    /// Return the OTS trajectory artifact outbox plus backend label.
    #[cfg_attr(not(feature = "observe"), allow(dead_code))]
    pub(crate) fn ots_trajectory_outbox(&self) -> Option<(&'static str, Arc<OtsTrajectoryOutbox>)> {
        let backend = self.storage_stack.as_ref()?.backend.as_str();
        let queue = self.ots_trajectory_outbox.lock().ok()?.clone()?;
        Some((backend, queue))
    }
    pub(crate) fn query_projection_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        fields: &serde_json::Value,
    ) -> serde_json::Value {
        let Some(obj) = fields.as_object() else {
            return fields.clone();
        };
        let registry = self.registry.read().unwrap();
        let Some(spec) = registry.get_spec(tenant, entity_type) else {
            return fields.clone();
        };

        let mut projected = obj.clone();
        for state_var in &spec.automaton.state {
            if state_var.query_indexed == Some(false) {
                projected.remove(&state_var.name);
            }
        }

        serde_json::Value::Object(projected)
    }

    pub(crate) fn query_projection_state(
        &self,
        state: &crate::entity_actor::EntityState,
    ) -> serde_json::Value {
        let mut projected = match serde_json::to_value(state) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    tenant = "unknown",
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    "failed to serialize query projection state"
                );
                return serde_json::json!({});
            }
        };
        if let Some(obj) = projected.as_object_mut() {
            obj.insert("events".to_string(), serde_json::json!([]));
        }
        projected
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

    /// Create ServerState from SpecRegistry using the PG-backed actor system.
    /// This is the actorized runtime path: actor_instances + actor_messages are source of truth.
    pub fn from_pg_registry(system: Arc<PgActorSystem>, registry: SpecRegistry) -> Self {
        let legacy = ActorSystem::new("pg-actor-compat");
        let mut state = Self::from_registry(legacy, registry);
        state.pg_actor_system = Some(system);
        state
    }

    /// Return true when OData for this tenant/entity should dispatch through
    /// the Postgres actor runtime.
    pub fn is_pg_actor_backed(&self, tenant: &TenantId, entity_type: &str) -> bool {
        self.actor_backed_types.contains(entity_type)
            || self
                .actor_backed_types
                .contains(&format!("{}:{entity_type}", tenant.as_str()))
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
    pub fn turso(&self) -> temper_store_turso::TursoEventStore {
        self.platform_turso_store()
            .expect("Turso event store is not configured")
    }

    /// Get an optional reference to the **platform** Turso event store.
    ///
    /// Only use for system-wide data that stays in the platform DB
    /// (evolution records, feature requests, tenant registry).
    /// For tenant-scoped data, use [`turso_store_for_tenant`].
    ///
    /// Returns `None` when the server is running without Turso (e.g. in-memory
    /// mode or tests). Callers should degrade gracefully to empty results.
    pub fn platform_turso_store(&self) -> Option<temper_store_turso::TursoEventStore> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.turso.as_ref())
            .and_then(|provider| provider.platform_store())
    }

    /// Get a Turso store for a specific tenant.
    ///
    /// In TenantRouted mode, routes to the per-tenant database.
    /// In single-DB Turso mode, returns the shared store.
    /// `temper-system` and `default` tenants route to the platform store.
    ///
    /// Returns `None` when the server is running without Turso.
    pub async fn turso_store_for_tenant(
        &self,
        tenant: &str,
    ) -> Option<temper_store_turso::TursoEventStore> {
        let provider = self.storage_stack.as_ref()?.turso.as_ref()?.clone();
        provider.store_for_tenant(tenant).await
    }

    /// Return a backend-neutral metadata store for one tenant.
    ///
    /// Postgres is a shared platform store with tenant columns; Turso may be
    /// single-DB or tenant-routed. This helper lets read/write paths avoid
    /// branching on Turso-only accessors.
    pub async fn metadata_store_for_tenant(&self, tenant: &str) -> Option<Arc<dyn MetadataStore>> {
        if let Some(provider) = self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.metadata.clone())
        {
            return provider.store_for_tenant(tenant).await;
        }

        None
    }

    /// Return the platform metadata store for system-wide tables.
    pub fn platform_metadata_store(&self) -> Option<Arc<dyn MetadataStore>> {
        if let Some(provider) = self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.metadata.clone())
        {
            return provider.platform_store();
        }

        None
    }

    /// Download stream content for a TemperFS `File` entity without going
    /// back through loopback HTTP.
    ///
    /// This is the programmatic equivalent of `GET /tdata/Files('{id}')/$value`
    /// and keeps WASM-local fast paths aligned with the normal blob_adapter
    /// read contract.
    pub async fn get_file_stream_content(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Result<(u16, Vec<u8>), String> {
        match self.read_file_stream_indexed(tenant, file_id).await? {
            IndexedFileStreamRead::Content { bytes, .. } => return Ok((200, bytes)),
            IndexedFileStreamRead::NoContent { .. } => return Ok((404, Vec::new())),
            IndexedFileStreamRead::MissingIndex => {
                tracing::warn!(
                    tenant = %tenant,
                    file_id,
                    "file stream projection missing; falling back to actor/WASM materialization"
                );
            }
            IndexedFileStreamRead::StaleIndex { content_hash, .. } => {
                tracing::warn!(
                    tenant = %tenant,
                    file_id,
                    content_hash = %content_hash,
                    "file stream projection blob missing; falling back to actor/WASM materialization"
                );
            }
        }

        let entity_state = serde_json::to_value(
            &self
                .get_tenant_entity_state(tenant, "File", file_id)
                .await
                .map_err(|e| format!("failed to load File('{file_id}') state: {e}"))?
                .state,
        )
        .map_err(|e| format!("failed to serialize File('{file_id}') state: {e}"))?;

        let has_content = entity_state
            .get("booleans")
            .and_then(|b| b.get("has_content"))
            .and_then(|v| v.as_bool())
            .or_else(|| {
                entity_state
                    .get("fields")
                    .and_then(|f| f.get("has_content"))
                    .and_then(|v| v.as_bool())
            })
            .unwrap_or(false);
        if !has_content {
            return Ok((404, Vec::new()));
        }

        let response_stream_id = format!("download-{}", temper_runtime::scheduler::sim_uuid());
        let streams = Arc::new(RwLock::new(StreamRegistry::default()));

        let inv_ctx = WasmInvocationContext {
            tenant: tenant.to_string(),
            entity_type: "File".to_string(),
            entity_id: file_id.to_string(),
            trigger_action: "StreamDownload".to_string(),
            wasm_module: Some("blob_adapter".to_string()),
            trigger_params: serde_json::json!({
                "stream_id": response_stream_id,
                "operation": "get",
            }),
            entity_state,
            agent_id: agent_ctx.agent_id.clone(),
            session_id: agent_ctx.session_id.clone(),
            integration_config: std::collections::BTreeMap::new(),
            trace_id: agent_ctx.trace_id.clone().unwrap_or_default(),
            workflow_root_entity_type: agent_ctx.workflow_root_entity_type.clone(),
            workflow_root_entity_id: agent_ctx.workflow_root_entity_id.clone(),
            workflow_run_id: agent_ctx.workflow_run_id.clone(),
            http_request: None,
        };

        let security_ctx = agent_ctx.security_ctx.as_ref().ok_or_else(|| {
            "blob_adapter requires the caller's authenticated security context".to_string()
        })?;
        let wasm_result = self
            .invoke_wasm_direct(
                tenant,
                "blob_adapter",
                inv_ctx,
                streams.clone(),
                security_ctx,
            )
            .await
            .map_err(|e| format!("blob_adapter download failed: {e}"))?;

        if !wasm_result.success {
            return Err(wasm_result
                .error
                .unwrap_or_else(|| "blob_adapter returned an unknown error".to_string()));
        }

        let bytes = streams
            .write()
            .map_err(|_| "stream registry lock was poisoned".to_string())?
            .take_stream(&response_stream_id)
            .unwrap_or_default();

        Ok((200, bytes))
    }
}
