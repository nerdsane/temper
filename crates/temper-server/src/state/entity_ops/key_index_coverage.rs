//! Durable key-index coverage and revision fencing.

use super::*;

impl ServerState {
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
            if !store.supports_authoritative_key_index() {
                self.key_index_watermarks_loaded
                    .write()
                    .expect("key index watermarks-loaded lock poisoned")
                    .insert(tenant.to_string());
                return;
            }
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
    /// versioned declared-key signature (see [`declared_key_set_signature`]).
    pub(crate) async fn key_index_backfill_covered_key_set(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Option<String> {
        if self.key_contract_activation_gated(tenant, entity_type) {
            return None;
        }
        self.ensure_key_index_watermarks_loaded(tenant).await;
        let cache_key = format!("{tenant}:{entity_type}");
        let (store, _) = self.event_journal()?;
        if !store.supports_authoritative_key_index() {
            return None;
        }

        // Live writes invalidate a stale durable watermark inside their journal
        // transaction. Refresh this exact decision before every keyed ownership
        // proof so an in-process A -> B -> A spec cycle cannot resurrect a cached A
        // watermark after the B write removed it.
        let covered = match store.key_index_backfilled_types(tenant.as_str()).await {
            Ok(types) => types
                .into_iter()
                .find_map(|(found_type, key_set)| (found_type == entity_type).then_some(key_set)),
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant, entity_type, error = %error,
                    "failed to refresh key-index coverage; treating type as scan-only"
                );
                None
            }
        };
        let mut cache = self
            .key_index_backfilled
            .write()
            .expect("key index backfilled lock poisoned");
        if let Some(key_set) = covered.as_ref() {
            cache.insert(cache_key, key_set.clone());
        } else {
            cache.remove(&cache_key);
        }
        covered
    }

    /// Whether `entity_key_index` is complete for `(tenant, entity_type)` under the
    /// CURRENT declared key-set — the ADR-0153/0171 backfill watermark. True only
    /// when the covered signature equals `current_key_set`, so a keyed hit or miss is
    /// trusted only after every currently-declared key is reconciled. Conservative on
    /// any failure (returns false → authoritative scan, never stale ownership).
    pub(crate) async fn key_index_backfill_complete(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        current_key_set: &str,
    ) -> bool {
        let Some((store, _)) = self.event_journal() else {
            return false;
        };
        if !store.supports_authoritative_key_index() {
            return false;
        }
        self.key_index_backfill_covered_key_set(tenant, entity_type)
            .await
            .as_deref()
            == Some(current_key_set)
    }

    /// Record (durably + in the read-path cache) that `entity_key_index` is complete
    /// for `(tenant, entity_type)` covering exactly `key_set` (the versioned declared-
    /// key signature). Called by the backfill once it has keyed every existing
    /// entity of the type, so subsequent keyed hits and misses are bounded without
    /// scanning (ADR-0153/0171). Overwrites any stale earlier signature.
    #[cfg(test)]
    pub(crate) async fn mark_key_index_backfilled(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        key_set: &str,
    ) {
        let Some((store, _)) = self.event_journal() else {
            return;
        };
        if !store.supports_authoritative_key_index() {
            return;
        }
        if let Err(e) = store
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

    /// Publish a backfill watermark only if no live write changed the type's key
    /// contract while replay was in progress. The cache changes only after the
    /// durable compare-and-set succeeds.
    pub(crate) async fn mark_key_index_backfilled_if_revision(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        key_set: &str,
        expected_revision: u64,
    ) -> Result<bool, PersistenceError> {
        let Some((store, _)) = self.event_journal() else {
            return Ok(false);
        };
        if !store.supports_authoritative_key_index() {
            return Ok(false);
        }
        let published = store
            .mark_key_index_backfilled_if_revision(
                tenant.as_str(),
                entity_type,
                key_set,
                expected_revision,
            )
            .await?;
        let cache_key = format!("{tenant}:{entity_type}");
        let mut cache = self
            .key_index_backfilled
            .write()
            .expect("key index backfilled lock poisoned");
        if published {
            cache.insert(cache_key, key_set.to_string());
        } else {
            cache.remove(&cache_key);
        }
        Ok(published)
    }
}
