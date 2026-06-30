//! ADR-0153 declared-key backfill: key `entity_key_index` for pre-existing entities
//! and record the per-(tenant, entity_type) watermark, so a keyed read MISS can mean
//! authoritative absence (retiring #324's full-type scan — the 413, ARN-68).

use temper_runtime::tenant::TenantId;

use crate::ServerState;
use crate::entity_actor::recover_entity_state_from_store;

use super::transition_table_for;

/// Backfill `entity_key_index` for existing entities, then record the watermark.
///
/// Enumeration is authoritative: keyed types come from the registry and their entity
/// ids from `store.list_entity_ids_by_type`. It must NOT read `state.entity_index`,
/// which is populated only when an actor spawns (lazy) and is therefore near-empty at
/// boot — the original bug that left ~0 of N entities keyed.
///
/// Each entity's current field state is taken from its snapshot, falling back to event
/// replay when no snapshot exists, so snapshot-less entities are still keyed — a gap
/// there would make the watermark unsound (a missed entity would read as absent). A
/// type is only watermarked when every entity was processed without an upsert failure.
/// Idempotent; entities written after the key was declared already co-commit their
/// keys at write time.
pub(in crate::state) async fn populate_key_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) {
    let Some((store, backend)) = state.event_journal() else {
        return;
    };

    // Keyed entity types from the registry — the authoritative record of what is
    // installed for this tenant (os-app entities live here, not in transition_tables).
    let keyed_types: Vec<(String, Vec<temper_jit::table::types::DeclaredKey>)> = {
        let registry = state.registry.read().unwrap();
        registry
            .entity_types(tenant)
            .into_iter()
            .filter_map(|entity_type| {
                let table = registry.get_table(tenant, entity_type)?;
                if table.keys.is_empty() {
                    None
                } else {
                    Some((entity_type.to_string(), table.keys.clone()))
                }
            })
            .collect()
    };

    for (entity_type, keys) in &keyed_types {
        // Already complete: the co-committed write path keeps the index whole, so
        // skip the re-scan.
        if state.key_index_backfill_complete(tenant, entity_type).await {
            continue;
        }

        let entity_ids = match store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to enumerate entities; type not watermarked"
                );
                continue;
            }
        };

        let table = transition_table_for(state, tenant, entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let total = entity_ids.len();
        let mut indexed = 0usize;
        let mut failed = false;

        for entity_id in &entity_ids {
            let fields = match current_entity_fields(
                tenant,
                entity_type,
                entity_id,
                table.as_ref(),
                &store,
                backend,
                blob_store.as_ref(),
            )
            .await
            {
                Some(fields) => fields,
                None => continue, // deleted / empty / unreplayable: nothing to key
            };
            let Some(field_map) = fields.as_object() else {
                continue;
            };
            let mut key_rows = Vec::new();
            for key in keys {
                if let Some(hash) =
                    crate::key_index::canonical_key_hash(&key.name, &key.properties, field_map)
                {
                    key_rows.push(temper_runtime::persistence::EntityKeyRow {
                        key_name: key.name.clone(),
                        key_hash: hash,
                    });
                }
            }
            if key_rows.is_empty() {
                continue;
            }
            match store
                .backfill_entity_keys(tenant.as_str(), entity_type, entity_id, &key_rows)
                .await
            {
                Ok(()) => indexed += 1,
                Err(e) => {
                    failed = true;
                    tracing::debug!(
                        error = %e, entity_type = %entity_type, entity_id = %entity_id,
                        "key index backfill: upsert failed"
                    );
                }
            }
            tokio::task::yield_now().await;
        }

        // Only declare the type complete (authoritative-absence eligible) if every
        // entity was keyed without failure — otherwise keyed misses keep scanning.
        if failed {
            tracing::warn!(
                tenant = %tenant, entity_type = %entity_type, total, indexed,
                "key index backfill: had upsert failures; type NOT watermarked (keyed misses keep scanning)"
            );
        } else {
            state.mark_key_index_backfilled(tenant, entity_type).await;
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type, total, indexed,
                "entity_key_index backfill complete; type watermarked"
            );
        }
    }
}

/// Current field state for one entity: snapshot if present, else event replay.
/// Returns `None` for deleted/empty/unreplayable entities (nothing to key). Replay
/// requires the transition table; without one, snapshot-less entities are skipped.
async fn current_entity_fields(
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    table: Option<&temper_jit::TransitionTable>,
    store: &crate::storage::BoxedEventStore,
    backend: crate::storage::BackendLabel,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> Option<serde_json::Value> {
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    if let Ok(Some((_seq, snapshot_bytes))) = store.load_snapshot(&persistence_id).await
        && let Ok(snap) =
            serde_json::from_slice::<crate::entity_actor::EntityState>(&snapshot_bytes)
    {
        if snap.status == "Deleted" {
            return None;
        }
        return Some(snap.fields);
    }
    // No snapshot — replay from the journal to recover current state.
    let table = table?;
    let replayed = recover_entity_state_from_store(
        tenant.as_str(),
        entity_type,
        entity_id,
        table,
        store,
        backend,
        &serde_json::json!({}),
        blob_store,
    )
    .await
    .ok()?;
    if replayed.total_event_count == 0 || replayed.status == "Deleted" {
        return None;
    }
    Some(replayed.fields)
}
