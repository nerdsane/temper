//! ADR-0153 declared-key backfill: key `entity_key_index` for pre-existing entities
//! and record the per-(tenant, entity_type) watermark, so a keyed read MISS can mean
//! authoritative absence (retiring #324's full-type scan — the 413, ARN-68).

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::{EntityLoadOutcome, load_entity_current_fields, transition_table_for};

/// Retire key-index authority for types whose current spec has no declared keys.
///
/// Hot-deploy paths run this narrower pass before changing the registry. It must not
/// attempt exact rebuilds for declarations that are still installed: a bad current
/// declaration (for example, one that exposes duplicate durable keys) must remain
/// removable by a corrective generation even when its exact reconciliation fails.
pub(in crate::state) async fn retire_removed_key_index_watermarks(
    state: &ServerState,
    tenant: &TenantId,
) -> bool {
    let Some((store, _)) = state.event_journal() else {
        return true;
    };
    if !store.has_authoritative_key_index() {
        return true;
    }

    let current_keyed_types = {
        let registry = state.registry.read().expect("spec registry lock poisoned");
        registry
            .entity_types(tenant)
            .into_iter()
            .filter(|entity_type| {
                registry
                    .get_table(tenant, entity_type)
                    .is_some_and(|table| !table.keys.is_empty())
            })
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    };

    let known_watermarks = match store.key_index_backfilled_types(tenant.as_str()).await {
        Ok(watermarks) => watermarks,
        Err(e) => {
            tracing::error!(
                tenant = %tenant, error = %e,
                "key index backfill: durable watermark unreadable; retirement not started"
            );
            return false;
        }
    };
    let empty_key_set = crate::key_index::declared_key_set_signature(&[]);
    let mut succeeded = true;
    for (entity_type, _) in known_watermarks {
        if current_keyed_types.contains(&entity_type) {
            continue;
        }
        let _retirement_fence = match store
            .acquire_projection_reconciliation_fence(tenant.as_str(), &entity_type)
            .await
        {
            Ok(fence) => fence,
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to fence removed-declaration watermark retirement"
                );
                continue;
            }
        };
        let still_covered = match store.key_index_backfilled_types(tenant.as_str()).await {
            Ok(watermarks) => watermarks
                .into_iter()
                .any(|(covered_type, _)| covered_type == entity_type),
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: removed-declaration watermark re-read failed"
                );
                continue;
            }
        };
        if !still_covered {
            continue;
        }
        state.invalidate_cached_key_index_backfilled(tenant, &entity_type);
        if let Err(e) = state
            .mark_key_index_backfilled(tenant, &entity_type, &empty_key_set)
            .await
        {
            succeeded = false;
            tracing::error!(
                tenant = %tenant, entity_type = %entity_type, error = %e,
                "key index backfill: failed to retire removed-declaration watermark"
            );
        }
    }

    succeeded
}

/// Backfill `entity_key_index` for existing entities, then record the watermark.
///
/// Enumeration is authoritative: keyed types come from the registry, and entity IDs
/// are the union of durable entities and existing key rows. The projection half is
/// required because durable enumeration can omit tombstones while their historical
/// key rows still exist. It must NOT read `state.entity_index`, which is lazy.
///
/// Robustness at scale (tenants hold 10k–100k+ entities of a keyed type):
/// - **Exact**: before stamping a versioned watermark, every durable or projected ID
///   is replayed and its complete key-row set is replaced. A failed/partial pass is
///   intentionally retried in full; skipping existing rows would preserve the stale
///   rows this reconciliation exists to remove.
/// - **Sound**: a type is watermarked only if EVERY existing entity was either keyed
///   or is definitively skippable (deleted/phantom). One entity that exists but
///   cannot be loaded fails the type — it is not watermarked, and keyed misses keep
///   scanning (correct, just not bounded) until the data issue is resolved. The
///   failing `entity_id`s are logged so the integrity problem is visible.
/// - **Fenced**: a backend-owned per-type fence is held from the definitive durable
///   watermark read through reconciliation and watermark commit. Other reconcilers
///   and live projection writes cannot overlap that interval.
/// - **Cooperative**: yields between entities and runs as a background task, so the
///   heavy types (e.g. SessionEntry) do not block boot or the smaller, higher-value
///   types. Types are processed in the registry's sorted order.
///
/// Idempotent; entities written after the key was declared already co-commit their
/// keys at write time.
pub(in crate::state) async fn populate_key_index_from_snapshots(
    state: &ServerState,
    tenant: &TenantId,
) -> bool {
    let mut succeeded = retire_removed_key_index_watermarks(state, tenant).await;
    let Some((store, backend)) = state.event_journal() else {
        return succeeded;
    };
    if !store.has_authoritative_key_index() {
        // A backend that does not co-commit keys on live writes can never make a
        // keyed miss authoritative. Do not perform no-op repairs or cache a false
        // watermark (Turso intentionally remains scan-safe here).
        return succeeded;
    }

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
        let _reconciliation_fence = match store
            .acquire_projection_reconciliation_fence(tenant.as_str(), entity_type)
            .await
        {
            Ok(fence) => fence,
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to acquire reconciliation fence"
                );
                continue;
            }
        };
        // Read the durable watermark only after acquiring the fence. A cached value
        // may predate another worker's completed pass; a read error is not absence
        // and must abort before any projection mutation.
        let covered = match store.key_index_backfilled_types(tenant.as_str()).await {
            Ok(types) => types.into_iter().find_map(|(covered_type, key_set)| {
                (covered_type == *entity_type).then_some(key_set)
            }),
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: durable watermark unreadable; reconciliation not started"
                );
                continue;
            }
        };
        // Already complete for the CURRENT declared key-set: the co-committed write path
        // keeps the index whole, so skip the re-scan.
        if covered.as_deref() == Some(current_key_set.as_str()) {
            state.cache_key_index_backfilled(tenant, entity_type, &current_key_set);
            continue;
        }
        // This worker is about to replace projection rows under a missing/stale
        // durable watermark. Drop any older in-process claim of current coverage
        // first, so concurrent keyed misses stay scan-safe throughout the repair and
        // after any partial failure. A successful durable mark restores the cache.
        state.invalidate_cached_key_index_backfilled(tenant, entity_type);
        // Any missing/different watermark requires a full exact reconciliation. The
        // signature version makes this happen once after upgrading from binaries that
        // could leave tombstone rows behind.
        if covered.is_some() {
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type,
                covered_key_set = covered.as_deref().unwrap_or(""),
                current_key_set = %current_key_set,
                "key index backfill: signature changed — exactly reconciling every durable or projected entity (one-time)"
            );
        }

        let mut entity_ids: BTreeSet<String> = match store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to enumerate entities; type not watermarked"
                );
                continue;
            }
        };
        let projected_ids = match store
            .keyed_entity_ids_for_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                succeeded = false;
                tracing::error!(
                    tenant = %tenant, entity_type = %entity_type, error = %e,
                    "key index backfill: failed to enumerate existing projection rows; type not watermarked"
                );
                continue;
            }
        };
        entity_ids.extend(projected_ids);

        let table = transition_table_for(state, tenant, entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let total = entity_ids.len();
        let mut desired_rows = BTreeMap::new();
        let mut reconciled = 0usize;
        let mut failed = 0usize;

        // Phase 1: replay every discovered entity without mutating the projection.
        // Holding the desired sets until all loads succeed prevents a partial read
        // failure from beginning a destructive type reconciliation.
        for entity_id in &entity_ids {
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
                    let mut key_rows = Vec::new();
                    if let Some(field_map) = fields.as_object() {
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
                    }
                    desired_rows.insert(entity_id.clone(), key_rows);
                }
                EntityLoadOutcome::Skip { .. } => {
                    // A tombstone or projection-only phantom has an empty desired set.
                    desired_rows.insert(entity_id.clone(), Vec::new());
                }
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
        let skipped = desired_rows.values().filter(|rows| rows.is_empty()).count();

        // Phase 2: purge every discovered entity before assigning any desired keys.
        // Otherwise a later-sorted stale holder can make an earlier live assignment
        // conflict, then disappear, leaving the key unassigned under a fresh watermark.
        if failed == 0 {
            for entity_id in desired_rows.keys() {
                if let Err(e) = store
                    .backfill_entity_keys(tenant.as_str(), entity_type, entity_id, &[])
                    .await
                {
                    failed += 1;
                    tracing::warn!(
                        error = %e, entity_type = %entity_type, entity_id = %entity_id,
                        "key index backfill: purge phase failed"
                    );
                }
                tokio::task::yield_now().await;
            }
        }

        // Phase 3: only after a clean type-wide purge, assign every non-empty current
        // key set in deterministic entity-id order. Genuine duplicate live keys retain
        // the existing deterministic first-holder behavior.
        if failed == 0 {
            for (entity_id, key_rows) in &desired_rows {
                if key_rows.is_empty() {
                    continue;
                }
                match store
                    .backfill_entity_keys(tenant.as_str(), entity_type, entity_id, key_rows)
                    .await
                {
                    Ok(()) => reconciled += 1,
                    Err(e) => {
                        failed += 1;
                        tracing::warn!(
                            error = %e, entity_type = %entity_type, entity_id = %entity_id,
                            "key index backfill: assignment phase failed"
                        );
                    }
                }
                tokio::task::yield_now().await;
            }
        }

        // Watermark only if nothing failed — every durable or projected entity was
        // exactly reconciled. Otherwise keyed misses keep scanning, and a later boot
        // retries the full exact pass.
        if failed == 0
            && let Err(e) = state
                .mark_key_index_backfilled(tenant, entity_type, &current_key_set)
                .await
        {
            failed += 1;
            tracing::error!(
                tenant = %tenant, entity_type = %entity_type, error = %e,
                "key index backfill: failed to persist watermark"
            );
        }
        if failed == 0 {
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type, key_set = %current_key_set,
                total, reconciled, skipped,
                "entity_key_index backfill complete; type watermarked"
            );
        } else {
            succeeded = false;
            tracing::warn!(
                tenant = %tenant, entity_type = %entity_type,
                total, reconciled, skipped, failed,
                "key index backfill: {failed} entities unresolved; type NOT watermarked (keyed misses keep scanning; will retry exact reconciliation)"
            );
        }
    }

    succeeded
}
