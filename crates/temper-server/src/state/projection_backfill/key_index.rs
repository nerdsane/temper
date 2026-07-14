//! ADR-0153 declared-key backfill: key `entity_key_index` for pre-existing entities
//! and record the per-(tenant, entity_type) watermark, so a keyed read MISS can mean
//! authoritative absence (retiring #324's full-type scan — the 413, ARN-68).

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::{EntityLoadOutcome, load_entity_current_fields, transition_table_for};

/// Backfill `entity_key_index` for existing entities, then record the watermark.
///
/// Enumeration is authoritative: keyed types come from the registry and their entity
/// ids from `store.list_entity_ids_by_type`. It must NOT read `state.entity_index`,
/// which is populated only when an actor spawns (lazy) and is therefore near-empty at
/// boot — the original bug that left ~0 of N entities keyed.
///
/// Robustness at scale (tenants hold 10k–100k+ entities of a keyed type):
/// - **Resumable**: already-keyed entities are skipped (the costly step is loading
///   each entity's state), so a re-run after a partial pass only processes the
///   remainder instead of re-loading all N.
/// - **Sound**: a type is watermarked only if EVERY existing entity was either keyed
///   or is definitively skippable (deleted/phantom). One entity that exists but
///   cannot be loaded fails the type — it is not watermarked, and keyed misses keep
///   scanning (correct, just not bounded) until the data issue is resolved. The
///   failing `entity_id`s are logged so the integrity problem is visible.
/// - **Cooperative**: yields between entities and runs as a background task, so the
///   heavy types (e.g. SessionEntry) do not block boot or the smaller, higher-value
///   types. Types are processed in the registry's sorted order.
///
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
        let current_key_set = crate::key_index::declared_key_set_signature(keys);
        let covered = state
            .key_index_backfill_covered_key_set(tenant, entity_type)
            .await;
        // Already complete for the CURRENT declared key-set: the co-committed write path
        // keeps the index whole, so skip the re-scan.
        if covered.as_deref() == Some(current_key_set.as_str()) {
            continue;
        }
        // A watermark that covered a DIFFERENT key-set means a key was declared after
        // the first backfill (e.g. Directory `ws_path` added after `name_parent`):
        // existing entities are keyed for the old keys but NOT the new one. Since
        // `keyed_entity_ids_for_type` is per-entity (any key), the resumability skip
        // would wrongly skip them, so force a full re-key that re-loads and re-keys
        // every entity under all currently-declared keys (idempotent upsert).
        let force_full_rekey = covered.is_some();
        if force_full_rekey {
            // One-time on the boot that first sees a changed key-set (incl. the 0011
            // migration, which stamps every existing watermark's key_set to ''). Logged
            // so this expected full-reload of the type is distinguishable in Datadog from
            // a pathology.
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type,
                covered_key_set = covered.as_deref().unwrap_or(""),
                current_key_set = %current_key_set,
                "key index backfill: declared key-set changed — re-keying every existing entity of this type (one-time)"
            );
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

        // Resumability: on a FIRST-TIME backfill, skip entities already keyed (avoids
        // re-loading their state). On a key-set change we must re-key already-keyed
        // entities with the new key, so process all.
        let already_keyed: BTreeSet<String> = if force_full_rekey {
            BTreeSet::new()
        } else {
            match store
                .keyed_entity_ids_for_type(tenant.as_str(), entity_type)
                .await
            {
                Ok(ids) => ids.into_iter().collect(),
                Err(_) => BTreeSet::new(), // cannot resume → process all (correct, slower)
            }
        };

        let table = transition_table_for(state, tenant, entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let total = entity_ids.len();
        let mut newly_keyed = 0usize;
        let mut already = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;

        for entity_id in &entity_ids {
            if already_keyed.contains(entity_id) {
                already += 1;
                continue;
            }
            match load_entity_current_fields(
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
                EntityLoadOutcome::Fields { fields, .. } => {
                    let Some(field_map) = fields.as_object() else {
                        skipped += 1;
                        continue;
                    };
                    let mut key_rows = Vec::new();
                    for key in keys {
                        if let Some(hash) = crate::key_index::canonical_key_hash(
                            &key.name,
                            &key.properties,
                            field_map,
                        ) {
                            key_rows.push(temper_runtime::persistence::EntityKeyRow {
                                key_name: key.name.clone(),
                                key_hash: hash,
                            });
                        }
                    }
                    if key_rows.is_empty() {
                        // No resolvable key (all key components absent/null) — the
                        // entity is not addressable by this key, so skipping is sound.
                        skipped += 1;
                        continue;
                    }
                    match store
                        .backfill_entity_keys(tenant.as_str(), entity_type, entity_id, &key_rows)
                        .await
                    {
                        Ok(()) => newly_keyed += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(
                                error = %e, entity_type = %entity_type, entity_id = %entity_id,
                                "key index backfill: upsert failed"
                            );
                        }
                    }
                }
                EntityLoadOutcome::Skip { .. } => skipped += 1,
                EntityLoadOutcome::LoadFailed => {
                    failed += 1;
                    tracing::warn!(
                        entity_type = %entity_type, entity_id = %entity_id,
                        "key index backfill: existing entity could not be loaded; type will NOT be watermarked (data integrity issue)"
                    );
                }
            }
            tokio::task::yield_now().await;
        }

        // Watermark only if nothing failed — every existing entity was keyed or is
        // definitively skippable. Otherwise keyed misses keep scanning (sound), and a
        // later boot resumes from the remainder.
        if failed == 0 {
            state
                .mark_key_index_backfilled(tenant, entity_type, &current_key_set)
                .await;
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type, key_set = %current_key_set,
                total, newly_keyed, already, skipped,
                "entity_key_index backfill complete; type watermarked"
            );
        } else {
            tracing::warn!(
                tenant = %tenant, entity_type = %entity_type,
                total, newly_keyed, already, skipped, failed,
                "key index backfill: {failed} entities unresolved; type NOT watermarked (keyed misses keep scanning; will resume next run)"
            );
        }
    }
}
