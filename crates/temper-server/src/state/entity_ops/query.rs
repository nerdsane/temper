//! Read entity state and list ids.

use super::helpers::is_deleted_envelope;
use crate::entity_actor::{EntityResponse, InProcessEntityRuntime};
use crate::state::ServerState;
use crate::state::dispatch::retry;
use temper_runtime::plug::RuntimeRequest;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

impl ServerState {
    /// List entity IDs currently in the in-memory index for this type.
    pub fn list_entity_ids(&self, tenant: &TenantId, entity_type: &str) -> Vec<String> {
        let index_key = format!("{tenant}:{entity_type}");
        let index = self.entity_index.read().unwrap();
        index
            .get(&index_key)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get the current state of an entity actor (default tenant).
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
        let runtime = InProcessEntityRuntime::new(actor_ref);
        retry::execute_with_backoff(&runtime, RuntimeRequest::GetState, &policy)
            .await
            .result
            .map_err(|e| format!("Actor query failed: {e}"))
    }

    /// Whether the in-memory index already knows this entity.
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
        let runtime = InProcessEntityRuntime::new(actor_ref.clone());
        let outcome =
            retry::execute_with_backoff(&runtime, RuntimeRequest::GetState, &policy).await;
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

    /// Resolve the current status of an entity.
    ///
    /// Fast path: check the `entity_state_cache`.
    /// Slow path: fall back to `get_tenant_entity_state` and backfill the cache.
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
