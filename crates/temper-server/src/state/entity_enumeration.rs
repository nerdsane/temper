//! Shared fail-closed classification of raw store discovery candidates.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::persistence::{
    LATEST_EVENT_BATCH_SIZE, PersistenceError, is_deletion_tombstone,
};
use temper_runtime::tenant::TenantId;

use crate::storage::BoxedEventStore;
use crate::{
    entity_actor::{EntityMsg, EntityResponse},
    runtime_metrics,
};

use super::{ServerState, dispatch::retry};

const ENTITY_DISCOVERY_BUDGET: usize = 100_000;

mod lifecycle;
pub(super) use lifecycle::{read_latest_entity_lifecycle, read_latest_entity_lifecycles};

impl ServerState {
    pub(super) fn tenant_entity_index_epoch_key(tenant: &TenantId) -> String {
        format!("tenant:{tenant}")
    }

    pub(super) fn type_entity_index_epoch_key(tenant: &TenantId, entity_type: &str) -> String {
        format!("type:{tenant}:{entity_type}")
    }

    /// Apply one synchronous index mutation and advance the publication epoch.
    ///
    /// All production create/delete/index writers go through this method. The
    /// epoch mutex is acquired before the index lock, matching scan publication.
    pub(crate) fn mutate_entity_index<R>(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        mutation: impl FnOnce(&mut BTreeMap<String, BTreeSet<String>>) -> R,
    ) -> Result<R, String> {
        let mut epoch = self
            .entity_index_epoch
            .lock()
            .map_err(|error| format!("entity index epoch lock poisoned: {error}"))?;
        let mut index = self
            .entity_index
            .write()
            .map_err(|error| format!("entity index lock poisoned: {error}"))?;
        let result = mutation(&mut index);
        for epoch_key in [
            Self::tenant_entity_index_epoch_key(tenant),
            Self::type_entity_index_epoch_key(tenant, entity_type),
        ] {
            let value = epoch.entry(epoch_key).or_default();
            *value = value.checked_add(1).expect("entity index epoch exhausted");
        }
        Ok(result)
    }

    pub(super) fn capture_entity_index_epoch(
        &self,
        epoch_key: &str,
    ) -> Result<u64, PersistenceError> {
        self.entity_index_epoch
            .lock()
            .map(|epochs| epochs.get(epoch_key).copied().unwrap_or(0))
            .map_err(|error| {
                PersistenceError::Storage(format!("entity index epoch lock poisoned: {error}"))
            })
    }

    /// Publish a classified scan and hydration watermark only when no index
    /// mutation occurred since `expected_epoch` was captured.
    pub(super) fn publish_entity_scan<R>(
        &self,
        epoch_key: &str,
        expected_epoch: u64,
        changed_epoch_keys: &[String],
        publish: impl FnOnce(&mut BTreeMap<String, BTreeSet<String>>, &mut BTreeSet<String>) -> R,
    ) -> Result<Option<R>, PersistenceError> {
        let mut epoch = self.entity_index_epoch.lock().map_err(|error| {
            PersistenceError::Storage(format!("entity index epoch lock poisoned: {error}"))
        })?;
        if epoch.get(epoch_key).copied().unwrap_or(0) != expected_epoch {
            return Ok(None);
        }
        let mut index = self.entity_index.write().map_err(|error| {
            PersistenceError::Storage(format!("entity index lock poisoned: {error}"))
        })?;
        let mut hydrated = self.entity_index_hydrated.write().map_err(|error| {
            PersistenceError::Storage(format!("entity index hydration lock poisoned: {error}"))
        })?;
        let result = publish(&mut index, &mut hydrated);
        for changed_key in changed_epoch_keys {
            let value = epoch.entry(changed_key.clone()).or_default();
            *value = value.checked_add(1).expect("entity index epoch exhausted");
        }
        Ok(Some(result))
    }
}

impl ServerState {
    fn invalidate_entity_index_hydration(
        &self,
        hydrated_keys: &BTreeSet<String>,
    ) -> Result<(), PersistenceError> {
        let mut hydrated = self.entity_index_hydrated.write().map_err(|error| {
            PersistenceError::Storage(format!("entity index hydration lock poisoned: {error}"))
        })?;
        for key in hydrated_keys {
            hydrated.remove(key);
        }
        Ok(())
    }

    async fn classify_and_publish_entity_index(
        &self,
        tenant: &TenantId,
        publish_hydration: bool,
    ) -> Result<(Vec<(String, String)>, BTreeSet<String>), PersistenceError> {
        let Some((store, _backend)) = self.event_journal() else {
            return Ok((Vec::new(), BTreeSet::new()));
        };

        let epoch_key = Self::tenant_entity_index_epoch_key(tenant);
        let epoch = self.capture_entity_index_epoch(&epoch_key)?;
        let entities = store
            .list_entity_ids_limited(
                tenant.as_str(),
                None,
                ENTITY_DISCOVERY_BUDGET.saturating_add(1),
            )
            .await?;
        ensure_discovery_budget("tenant", tenant.as_str(), entities.len())?;
        let live_entities = live_entity_candidates(&store, tenant, &entities).await?;
        let mut hydrated_keys = entities
            .iter()
            .map(|(entity_type, _)| format!("{tenant}:{entity_type}"))
            .collect::<BTreeSet<_>>();
        let tenant_index_prefix = format!("{tenant}:");
        hydrated_keys.extend(
            self.entity_index
                .read()
                .map_err(|error| {
                    PersistenceError::Storage(format!("entity index lock poisoned: {error}"))
                })?
                .keys()
                .filter(|key| key.starts_with(&tenant_index_prefix))
                .cloned(),
        );
        hydrated_keys.extend(
            self.registry
                .read()
                .map_err(|error| {
                    PersistenceError::Storage(format!("registry lock poisoned: {error}"))
                })?
                .entity_types(tenant)
                .into_iter()
                .map(|entity_type| format!("{tenant}:{entity_type}")),
        );
        let mut changed_epoch_keys = vec![epoch_key.clone()];
        changed_epoch_keys.extend(
            hydrated_keys
                .iter()
                .filter_map(|index_key| index_key.strip_prefix(&tenant_index_prefix))
                .map(|entity_type| Self::type_entity_index_epoch_key(tenant, entity_type)),
        );
        let published =
            self.publish_entity_scan(&epoch_key, epoch, &changed_epoch_keys, |index, hydrated| {
                // Epoch validation proves no in-scope mutation occurred since
                // enumeration began, so replace each scanned type wholesale.
                // Removing only rediscovered candidates would preserve a stale
                // in-memory id whose journal disappeared and could let a later
                // consumer bootstrap it as a new entity.
                for index_key in &hydrated_keys {
                    index.remove(index_key);
                }
                for (entity_type, entity_id) in &live_entities {
                    index
                        .entry(format!("{tenant}:{entity_type}"))
                        .or_default()
                        .insert(entity_id.clone());
                }
                if publish_hydration {
                    hydrated.extend(hydrated_keys.iter().cloned());
                } else {
                    for key in &hydrated_keys {
                        hydrated.remove(key);
                    }
                }
            })?;
        if published.is_none() {
            return Err(PersistenceError::Storage(
                "entity index changed while durable candidates were classified; retrying is required"
                    .to_string(),
            ));
        }
        Ok((live_entities, hydrated_keys))
    }

    /// Populate `entity_index` from the event store without spawning actors.
    #[tracing::instrument(skip_all, fields(otel.name = "entity.populate_index_from_store", tenant = %tenant))]
    pub async fn populate_index_from_store(
        &self,
        tenant: &TenantId,
    ) -> Result<usize, PersistenceError> {
        let (live_entities, _) = self.classify_and_publish_entity_index(tenant, true).await?;
        let live_count = live_entities.len();
        tracing::info!(tenant = %tenant, count = live_count, "populated entity index from event store");
        runtime_metrics::record_server_state_metrics(self);
        Ok(live_count)
    }

    /// Populate `entity_index` for one entity type from the event store.
    #[tracing::instrument(skip_all, fields(
        otel.name = "entity.populate_index_from_store_by_type",
        tenant = %tenant,
        entity_type,
    ))]
    pub async fn populate_index_from_store_by_type(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<usize, PersistenceError> {
        let Some((store, _backend)) = self.event_journal() else {
            return Ok(0);
        };

        let epoch_key = Self::type_entity_index_epoch_key(tenant, entity_type);
        let epoch = self.capture_entity_index_epoch(&epoch_key)?;
        let candidates = store
            .list_entity_ids_limited(
                tenant.as_str(),
                Some(entity_type),
                ENTITY_DISCOVERY_BUDGET.saturating_add(1),
            )
            .await?;
        ensure_discovery_budget(entity_type, tenant.as_str(), candidates.len())?;
        let live = live_entity_candidates(&store, tenant, &candidates).await?;
        let count = live.len();
        let changed_epoch_keys = [
            epoch_key.clone(),
            Self::tenant_entity_index_epoch_key(tenant),
        ];
        let published =
            self.publish_entity_scan(&epoch_key, epoch, &changed_epoch_keys, |index, hydrated| {
                let index_key = format!("{tenant}:{entity_type}");
                index.remove(&index_key);
                for (_, entity_id) in live {
                    index
                        .entry(index_key.clone())
                        .or_default()
                        .insert(entity_id);
                }
                hydrated.insert(index_key);
            })?;
        if published.is_none() {
            return Err(PersistenceError::Storage(format!(
                "entity index for {tenant}:{entity_type} changed during durable classification; retrying is required"
            )));
        }
        tracing::info!(tenant = %tenant, entity_type, count, "populated typed entity index from event store");
        runtime_metrics::record_server_state_metrics(self);
        Ok(count)
    }

    /// Hydrate all durable actors, publishing completeness only after full success.
    #[tracing::instrument(skip_all, fields(otel.name = "entity.hydrate_from_store", tenant = %tenant))]
    pub async fn hydrate_from_store(&self, tenant: &TenantId) -> Result<usize, PersistenceError> {
        if self.event_journal().is_none() {
            return Ok(0);
        }
        let (live_entities, hydrated_keys) = self
            .classify_and_publish_entity_index(tenant, false)
            .await?;
        let mut hydrated = 0usize;
        for (entity_type, entity_id) in &live_entities {
            let Some(actor_ref) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            else {
                self.invalidate_entity_index_hydration(&hydrated_keys)?;
                return Err(PersistenceError::Storage(format!(
                    "failed to spawn {tenant}:{entity_type}:{entity_id} during eager hydration"
                )));
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
                }
                Ok(_) => hydrated = hydrated.saturating_add(1),
                Err(error) => {
                    self.stop_and_remove_entity(tenant, entity_type, entity_id);
                    self.invalidate_entity_index_hydration(&hydrated_keys)?;
                    return Err(PersistenceError::Storage(format!(
                        "failed to hydrate {tenant}:{entity_type}:{entity_id}: {error}"
                    )));
                }
            }
        }
        self.entity_index_hydrated
            .write()
            .map_err(|error| {
                PersistenceError::Storage(format!("entity index hydration lock poisoned: {error}"))
            })?
            .extend(hydrated_keys);
        tracing::info!(
            tenant = %tenant,
            count = hydrated,
            discovered = live_entities.len(),
            "hydrated entities from event store"
        );
        runtime_metrics::record_server_state_metrics(self);
        Ok(hydrated)
    }

    /// Return the authoritative latest journal sequence for each live derived
    /// candidate. `None` means this state has no durable journal and callers
    /// must use its in-memory source of truth. Missing/tombstoned streams are
    /// omitted; storage errors propagate.
    pub(crate) async fn live_journal_candidate_sequences(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<BTreeMap<String, u64>>, PersistenceError> {
        let Some((store, _)) = self.event_journal() else {
            return Ok(None);
        };
        let mut live = BTreeMap::new();
        for chunk in entity_ids.chunks(LATEST_EVENT_BATCH_SIZE) {
            let persistence_ids = chunk
                .iter()
                .map(|entity_id| format!("{tenant}:{entity_type}:{entity_id}"))
                .collect::<Vec<_>>();
            let latest = read_latest_entity_lifecycles(&store, &persistence_ids).await?;
            if latest.len() != chunk.len() {
                return Err(PersistenceError::Storage(format!(
                    "latest-event read returned {} rows for {} derived candidates",
                    latest.len(),
                    chunk.len()
                )));
            }
            for (entity_id, event) in chunk.iter().zip(latest) {
                if let Some(event) = event
                    && !is_deletion_tombstone(&event.lifecycle_event)
                {
                    live.insert(entity_id.clone(), event.raw_sequence);
                }
            }
        }
        Ok(Some(live))
    }
}

pub(super) async fn live_entity_candidates(
    store: &BoxedEventStore,
    tenant: &TenantId,
    candidates: &[(String, String)],
) -> Result<Vec<(String, String)>, PersistenceError> {
    let mut live = Vec::with_capacity(candidates.len());
    for chunk in candidates.chunks(LATEST_EVENT_BATCH_SIZE) {
        let persistence_ids = chunk
            .iter()
            .map(|(entity_type, entity_id)| format!("{tenant}:{entity_type}:{entity_id}"))
            .collect::<Vec<_>>();
        let latest_events = read_latest_entity_lifecycles(store, &persistence_ids).await?;
        if latest_events.len() != chunk.len() {
            return Err(PersistenceError::Storage(format!(
                "latest-event read returned {} rows for {} candidates",
                latest_events.len(),
                chunk.len()
            )));
        }
        for (candidate, latest_event) in chunk.iter().zip(latest_events) {
            let latest_event = latest_event.as_ref().ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "discovered entity stream has no journal tail: {tenant}:{}:{}",
                    candidate.0, candidate.1
                ))
            })?;
            if !is_deletion_tombstone(&latest_event.lifecycle_event) {
                live.push(candidate.clone());
            }
        }
    }
    Ok(live)
}

fn ensure_discovery_budget(
    scope: &str,
    tenant: &str,
    candidate_count: usize,
) -> Result<(), PersistenceError> {
    if candidate_count > ENTITY_DISCOVERY_BUDGET {
        return Err(PersistenceError::Storage(format!(
            "entity discovery budget exceeded for {tenant}:{scope}: more than {ENTITY_DISCOVERY_BUDGET} candidates"
        )));
    }
    Ok(())
}

pub(super) async fn bounded_entity_ids_by_type(
    store: &BoxedEventStore,
    tenant: &TenantId,
    entity_type: &str,
) -> Result<Vec<String>, PersistenceError> {
    let candidates = store
        .list_entity_ids_limited(
            tenant.as_str(),
            Some(entity_type),
            ENTITY_DISCOVERY_BUDGET.saturating_add(1),
        )
        .await?;
    ensure_discovery_budget(entity_type, tenant.as_str(), candidates.len())?;
    Ok(candidates
        .into_iter()
        .map(|(_, entity_id)| entity_id)
        .collect())
}

#[cfg(test)]
mod tests;
