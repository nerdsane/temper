use temper_runtime::tenant::TenantId;

use crate::entity_actor::recover_entity_state_from_store;
use crate::runtime_metrics;

use super::ServerState;

pub(super) async fn populate_field_index_from_snapshots(state: &ServerState, tenant: &TenantId) {
    let Some((store, backend)) = state.event_journal() else {
        return;
    };
    let Some(query_plane) = state.query_plane_store() else {
        return;
    };

    let entities = {
        let index = state.entity_index.read().unwrap();
        let mut result = Vec::new();
        for (index_key, ids) in index.iter() {
            let prefix = format!("{tenant}:");
            if let Some(entity_type) = index_key.strip_prefix(&prefix) {
                for id in ids {
                    result.push((entity_type.to_string(), id.clone()));
                }
            }
        }
        result
    };

    let total = entities.len();
    let mut indexed = 0usize;
    let mut errors = 0usize;
    let mut needs_replay = Vec::new();

    for (entity_type, entity_id) in &entities {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        match store.load_snapshot(&persistence_id).await {
            Ok(Some((_seq, snapshot_bytes))) => {
                if let Ok(state_snapshot) =
                    serde_json::from_slice::<crate::entity_actor::EntityState>(&snapshot_bytes)
                {
                    if let Err(e) = query_plane
                        .upsert_projection(
                            tenant.as_str(),
                            entity_type,
                            entity_id,
                            &state_snapshot.status,
                            &state.query_projection_fields(
                                tenant,
                                entity_type,
                                &state_snapshot.fields,
                            ),
                            state_snapshot.sequence_nr,
                        )
                        .await
                    {
                        tracing::debug!(
                            error = %e,
                            entity_type = %entity_type,
                            entity_id = %entity_id,
                            "field index backfill: upsert failed"
                        );
                        errors += 1;
                    } else {
                        indexed += 1;
                    }
                }
            }
            Ok(None) => {
                needs_replay.push((entity_type.clone(), entity_id.clone()));
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "field index backfill: snapshot load failed"
                );
                errors += 1;
            }
        }
    }

    tracing::info!(
        tenant = %tenant,
        total,
        snapshot_indexed = indexed,
        needs_replay = needs_replay.len(),
        "field index phase 1 (snapshots) complete, starting phase 2 (persistence replay)"
    );
    if !needs_replay.is_empty() {
        runtime_metrics::record_projection_backfill_snapshot_misses(
            tenant.as_str(),
            needs_replay.len() as u64,
        );
    }

    for (entity_type, entity_id) in &needs_replay {
        let table = {
            let registry = state.registry.read().unwrap();
            registry
                .get_table_live(tenant, entity_type)
                .map(|table| table.read().expect("table lock poisoned").clone())
        }
        .or_else(|| {
            state
                .transition_tables
                .get(entity_type)
                .map(|table| (**table).clone())
        });

        let Some(table) = table else {
            tracing::debug!(
                entity_type = %entity_type,
                entity_id = %entity_id,
                "field index backfill: no transition table available for replay"
            );
            errors += 1;
            continue;
        };

        let tenant_blob_store = state.blob_store_for_tenant(tenant).ok();
        let replayed = recover_entity_state_from_store(
            tenant.as_str(),
            entity_type,
            entity_id,
            &table,
            &store,
            backend,
            &serde_json::json!({}),
            tenant_blob_store.as_ref(),
        )
        .await;

        if replayed.total_event_count == 0 {
            tracing::debug!(
                entity_type = %entity_type,
                entity_id = %entity_id,
                "field index backfill: persistence replay produced no state"
            );
            errors += 1;
        } else if replayed.status == "Deleted" {
            if let Err(e) = query_plane
                .remove_projection(tenant.as_str(), entity_type, entity_id)
                .await
            {
                tracing::debug!(
                    error = %e,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "field index backfill: failed to clear deleted projection"
                );
                errors += 1;
            }
        } else {
            match query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &replayed.status,
                    &state.query_projection_fields(tenant, entity_type, &replayed.fields),
                    replayed.sequence_nr,
                )
                .await
            {
                Ok(()) => {
                    indexed += 1;
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "field index backfill: replay upsert failed"
                    );
                    errors += 1;
                }
            }
        }

        tokio::task::yield_now().await;
    }

    tracing::info!(
        tenant = %tenant,
        total,
        indexed,
        errors,
        "populated query projections (snapshots + persistence replay)"
    );
}
