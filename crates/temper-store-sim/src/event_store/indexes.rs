//! Deterministic simulation event-store indexes operations.

use super::*;

impl SimEventStore {
    pub(super) async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        for row in key_rows {
            let slot = (
                tenant.to_string(),
                entity_type.to_string(),
                row.key_name.clone(),
                row.key_hash.clone(),
            );
            match inner.key_index.get(&slot) {
                // A different entity holds it — pre-existing conflict; skip (don't
                // clobber, don't fail the backfill).
                Some(existing) if existing.as_str() != entity_id => continue,
                _ => {
                    inner.key_index.retain(|(t, et, kn, _), eid| {
                        !(t.as_str() == tenant
                            && et.as_str() == entity_type
                            && kn.as_str() == row.key_name.as_str()
                            && eid.as_str() == entity_id)
                    });
                    inner.key_index.insert(slot, entity_id.to_string());
                }
            }
        }
        Ok(())
    }

    pub(super) async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let slot = (
            tenant.to_string(),
            entity_type.to_string(),
            key_name.to_string(),
            key_hash.to_string(),
        );
        Ok(inner.key_index.get(&slot).cloned())
    }

    pub(super) async fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        // Overwrite the covered key-set (a re-key after a key-set change replaces the
        // stale set), mirroring the Postgres upsert.
        inner.key_index_watermark.insert(
            (tenant.to_string(), entity_type.to_string()),
            key_set.to_string(),
        );
        Ok(())
    }

    pub(super) async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner
            .key_index_watermark
            .iter()
            .filter(|((t, _), _)| t.as_str() == tenant)
            .map(|((_, et), key_set)| (et.clone(), key_set.clone()))
            .collect())
    }

    pub(super) async fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for ((t, et, _, _), entity_id) in inner.key_index.iter() {
            if t.as_str() == tenant && et.as_str() == entity_type {
                ids.insert(entity_id.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }

    pub(super) async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        // Reconcile: drop ALL of the entity's rows, then insert the current ones.
        // Empty `vector_rows` purges the entity (deleted / un-embedded). Idempotent.
        inner.vector_index.retain(|(t, et, _, _, eid), _| {
            !(t.as_str() == tenant && et.as_str() == entity_type && eid == entity_id)
        });
        for row in vector_rows {
            inner.vector_index.insert(
                (
                    tenant.to_string(),
                    entity_type.to_string(),
                    row.decl_name.clone(),
                    row.model_tag.clone(),
                    entity_id.to_string(),
                ),
                row.vector.clone(),
            );
        }
        Ok(())
    }

    pub(super) async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<EntityVectorCandidate>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        // BTreeMap iteration is ordered by key, so `entity_id` (the last key
        // component within a fixed partition) yields deterministic candidate order.
        // Cap at `limit` so an over-budget partition is detected without copying it all.
        let mut out = Vec::new();
        for ((t, et, decl, tag, entity_id), vector) in inner.vector_index.iter() {
            if t.as_str() == tenant
                && et.as_str() == entity_type
                && decl.as_str() == decl_name
                && tag.as_str() == model_tag
            {
                if out.len() >= limit {
                    break;
                }
                out.push(EntityVectorCandidate {
                    entity_id: entity_id.clone(),
                    vector: vector.clone(),
                });
            }
        }
        Ok(out)
    }

    pub(super) async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.vector_index_watermark.insert(
            (tenant.to_string(), entity_type.to_string()),
            vector_set.to_string(),
        );
        Ok(())
    }

    pub(super) async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner
            .vector_index_watermark
            .iter()
            .filter(|((t, _), _)| t.as_str() == tenant)
            .map(|((_, et), vector_set)| (et.clone(), vector_set.clone()))
            .collect())
    }

    pub(super) async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for ((t, et, _, _, entity_id), _) in inner.vector_index.iter() {
            if t.as_str() == tenant && et.as_str() == entity_type {
                ids.insert(entity_id.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }
}
