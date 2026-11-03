//! Repair interrupted projection writes before requests can observe cold rows.
use super::transition_table_for;
use crate::entity_actor::{EntityState, recover_entity_state_for_walk};
use crate::state::ServerState;
use std::collections::BTreeMap;
use std::time::Duration;
use temper_runtime::persistence::{PersistenceError, ProjectionRepairCandidate};
use temper_runtime::tenant::TenantId;
use tracing::{Instrument, instrument};

impl ServerState {
    /// Repair every serving tenant, including apps restored from the store.
    /// Registry membership is authoritative here, not --app or connected_tenants
    /// (the latter is empty on a single Turso store). An unavailable replay spec
    /// withholds its tenant from serving while other tenants continue booting.
    /// Other failed repairs keep boot pending; a server must not expose a row
    /// it knows is behind its journal.
    #[instrument(skip_all, fields(otel.name = "boot.projection_repair"))]
    pub(crate) async fn repair_query_projections_at_boot(&self) -> Vec<(TenantId, EntityState)> {
        if self.query_plane_store().is_none() || self.event_journal().is_none() {
            return Vec::new();
        }
        let tenants: Vec<_> = self
            .registry
            .read()
            .expect("registry lock poisoned")
            .tenant_ids()
            .into_iter()
            .cloned()
            .collect();
        let mut repaired = Vec::new();
        for tenant in tenants {
            let mut delay = Duration::from_secs(1);
            let mut pending = loop {
                match self.projection_repair_candidates(&tenant).await {
                    Ok(rows) => break rows,
                    Err(error) => {
                        tracing::error!(%tenant, %error, "boot projection comparison failed; tenant cannot serve yet");
                        tokio::time::sleep(delay).await; // determinism-ok: boot retry on runtime clock
                        delay = (delay * 2).min(Duration::from_secs(60));
                    }
                }
            };
            {
                let mut registry = self.registry.write().expect("registry lock poisoned");
                // Replace-mode installs retire a type's spec and routes but keep
                // its journal and catalog. Those histories no longer serve.
                // Keep types still exposed by CSDL even if their spec failed to
                // restore: they must withhold the tenant below, not serve stale rows.
                pending.retain(|candidate| {
                    let entity_type = &candidate.journal.entity_type;
                    registry.get_tenant(&tenant).is_some_and(|config| {
                        config.entities.contains_key(entity_type)
                            || config.entity_set_map.values().any(|ty| ty == entity_type)
                    }) || self.transition_tables.contains_key(entity_type)
                });
                // Match transition_table_for's registry and legacy table lookup.
                if let Some(candidate) = pending.iter().find(|candidate| {
                    registry
                        .get_table_live(&tenant, &candidate.journal.entity_type)
                        .is_none()
                        && !self
                            .transition_tables
                            .contains_key(&candidate.journal.entity_type)
                }) {
                    let entity_type = &candidate.journal.entity_type;
                    let reason = format!(
                        "Boot projection repair cannot replay type '{entity_type}': its spec is unavailable"
                    );
                    tracing::error!(%tenant, %entity_type,
                        entity_id = %candidate.journal.entity_id, %reason,
                        "tenant withheld from serving; other tenants continue booting");
                    // Restoration can omit an unreadable type while keeping
                    // its CSDL routes. Withhold the whole affected tenant via
                    // the existing 503 gate, just as a failed tenant restore
                    // does. Removing only the in-memory registration also
                    // excludes it from timer recovery and background walks;
                    // all durable specs, journals and rows remain untouched.
                    registry.mark_tenant_unavailable(tenant.clone(), reason);
                    registry.remove_tenant(&tenant);
                    continue;
                }
            }
            // Only lagging records load snapshots/events. Healthy records never
            // acquire actors, replay their history or rewrite their projections.
            let replay = tracing::info_span!("boot.projection_replay", tenant = %tenant, records = pending.len());
            async {
                while !pending.is_empty() {
                    let mut retry = Vec::new();
                    for candidate in pending {
                        match self.repair_query_projection(&tenant, &candidate).await {
                            Ok(current) => repaired.push((tenant.clone(), current)),
                            Err(error) => {
                                tracing::error!(%tenant, entity_type = %candidate.journal.entity_type,
                                    entity_id = %candidate.journal.entity_id, %error,
                                    "boot projection repair failed; tenant cannot serve yet");
                                retry.push(candidate);
                            }
                        }
                    }
                    pending = retry;
                    if !pending.is_empty() {
                        tokio::time::sleep(delay).await; // determinism-ok: boot retry on runtime clock
                        delay = (delay * 2).min(Duration::from_secs(60));
                    }
                }
            }
            .instrument(replay)
            .await;
        }
        repaired
    }

    #[instrument(skip_all, fields(otel.name = "boot.projection_compare", tenant = %tenant))]
    async fn projection_repair_candidates(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<ProjectionRepairCandidate>, PersistenceError> {
        let stack = self
            .storage_stack
            .as_ref()
            .expect("repair requires a stack");
        let plane = self
            .query_plane_store()
            .expect("repair requires a query plane");
        if stack.query_plane_is_journal()
            && let Some(rows) = plane.lagging_projections(tenant.as_str()).await?
        {
            return Ok(rows);
        }
        // Separate stores cannot join each other's tables. Two metadata batches
        // replace the native comparison query; no per-record freshness reads and
        // no healthy entity bodies are loaded on this path either.
        let heads = stack.events.journal_heads(tenant.as_str()).await?;
        let mut checkpoints: BTreeMap<_, _> = plane
            .projection_checkpoints(tenant.as_str())
            .await?
            .into_iter()
            .map(|p| ((p.entity_type.clone(), p.entity_id.clone()), p))
            .collect();
        Ok(heads
            .into_iter()
            .filter_map(|journal| {
                let projection =
                    checkpoints.remove(&(journal.entity_type.clone(), journal.entity_id.clone()));
                let behind = projection
                    .as_ref()
                    .map_or(!journal.deleted, |p| p.sequence_nr < journal.sequence_nr);
                behind.then_some(ProjectionRepairCandidate {
                    journal,
                    projection,
                })
            })
            .collect())
    }

    #[instrument(skip_all, fields(otel.name = "boot.projection_replay_record", tenant = %tenant,
        entity_type = %candidate.journal.entity_type, entity_id = %candidate.journal.entity_id))]
    async fn repair_query_projection(
        &self,
        tenant: &TenantId,
        candidate: &ProjectionRepairCandidate,
    ) -> Result<EntityState, String> {
        let head = &candidate.journal;
        let table = transition_table_for(self, tenant, &head.entity_type)
            .ok_or_else(|| format!("no replay table for {}", head.entity_type))?;
        let (store, backend) = self.event_journal().expect("repair requires a journal");
        let blobs = self.blob_store_for_tenant(tenant).ok();
        let current = recover_entity_state_for_walk(
            tenant.as_str(),
            &head.entity_type,
            &head.entity_id,
            &table,
            &store,
            backend,
            &serde_json::json!({}),
            blobs.as_ref(),
        )
        .await
        .map_err(|e| e.to_string())?;
        if current.sequence_nr < head.sequence_nr {
            return Err(format!(
                "replay stopped at {}, journal comparison saw {}",
                current.sequence_nr, head.sequence_nr
            ));
        }
        let plane = self
            .query_plane_store()
            .expect("repair requires a query plane");
        if current.is_tombstoned() {
            plane
                .remove_projection(tenant.as_str(), &head.entity_type, &head.entity_id)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            plane
                .upsert_projection(
                    tenant.as_str(),
                    &head.entity_type,
                    &head.entity_id,
                    &current.status,
                    &self.query_projection_fields(tenant, &head.entity_type, &current.fields),
                    &self.query_projection_state(&current),
                    current.sequence_nr,
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        // WARN is exported by deployed kernels; this is the per-record audit
        // needed to identify already-split records after the roll.
        tracing::warn!(%tenant, entity_type = %head.entity_type, entity_id = %head.entity_id,
            row_sequence = ?candidate.projection.as_ref().map(|p| p.sequence_nr),
            row_state = ?candidate.projection.as_ref().map(|p| p.status.as_str()),
            journal_sequence = current.sequence_nr, journal_state = %current.status,
            "repaired stored row from journal at boot");
        Ok(current)
    }
}
