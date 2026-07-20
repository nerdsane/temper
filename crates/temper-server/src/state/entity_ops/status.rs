//! Cached and actor-backed entity status resolution.

use super::*;

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
