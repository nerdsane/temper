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
/// - **Resumable, with a healing exception (ARN-238)**: every entity is loaded
///   (pre-watermark only — the watermark still ends all re-runs), because an
///   already-keyed entity may hold STALE ownership: a tombstone, or a key whose
///   components were nulled before release-on-delete/null existed. Living,
///   fully-resolvable already-keyed entities skip the index write, so a resumed
///   pass re-loads but does not re-write the finished remainder.
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
        // `membership_known` tracks whether the empty/filled set is AUTHORITATIVE.
        // On force_full_rekey and on a membership fetch error the set is empty by
        // construction, which must mean "process everything" — including releasing
        // stale rows of entities whose keys are all unresolvable — not "known to
        // be unindexed" (ARN-238: skipping them here would let the watermark end
        // their healing silently).
        let (already_keyed, membership_known): (BTreeSet<String>, bool) = if force_full_rekey {
            (BTreeSet::new(), false)
        } else {
            match store
                .keyed_entity_ids_for_type(tenant.as_str(), entity_type)
                .await
            {
                Ok(ids) => (ids.into_iter().collect(), true),
                Err(_) => (BTreeSet::new(), false), // cannot resume → process all
            }
        };

        let table = transition_table_for(state, tenant, entity_type);
        let blob_store = state.blob_store_for_tenant(tenant).ok();
        let total = entity_ids.len();
        let mut newly_keyed = 0usize;
        let mut healed = 0usize;
        let mut already = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;

        for entity_id in &entity_ids {
            // ARN-238: an already-keyed entity cannot be fast-skipped without
            // loading it — its rows may be STALE ownership from a delete that
            // predates release-on-delete, and healing requires seeing the
            // tombstone. Living already-keyed entities still skip the upsert
            // below; only the cheap membership shortcut moved.
            let was_already_keyed = already_keyed.contains(entity_id);
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
                EntityLoadOutcome::Fields(fields) => {
                    let Some(field_map) = fields.as_object() else {
                        skipped += 1;
                        continue;
                    };
                    // One row per declared key: a real hash when the key resolves,
                    // a RELEASE marker when it does not (ARN-238 — a key whose
                    // components were nulled must stop owning its old value; the
                    // stale row is exactly why the entity looks already-keyed).
                    let mut any_release = false;
                    let mut any_hash = false;
                    let key_rows: Vec<temper_runtime::persistence::EntityKeyRow> = keys
                        .iter()
                        .map(|key| {
                            let hash = crate::key_index::canonical_key_hash(
                                &key.name,
                                &key.properties,
                                field_map,
                            );
                            match hash {
                                Some(hash) => {
                                    any_hash = true;
                                    temper_runtime::persistence::EntityKeyRow {
                                        key_name: key.name.clone(),
                                        key_hash: hash,
                                    }
                                }
                                None => {
                                    any_release = true;
                                    temper_runtime::persistence::EntityKeyRow {
                                        key_name: key.name.clone(),
                                        key_hash: String::new(),
                                    }
                                }
                            }
                        })
                        .collect();
                    if was_already_keyed && !any_release {
                        // Alive, fully resolvable, already indexed — nothing to write.
                        already += 1;
                        continue;
                    }
                    if !any_hash && !was_already_keyed && membership_known {
                        // Authoritatively not indexed and nothing resolvable — the
                        // entity is not addressable by any declared key. Without
                        // membership authority we fall through and write the
                        // release markers (bounded no-op deletes), because the
                        // entity may hold stale rows we cannot see from here.
                        skipped += 1;
                        continue;
                    }
                    match store
                        .backfill_entity_keys(tenant.as_str(), entity_type, entity_id, &key_rows)
                        .await
                    {
                        // A write with at least one real hash is a claim; a
                        // release-only write is a heal — kept separate so the
                        // completion log's newly_keyed stays an honest claim count.
                        Ok(()) if any_hash => newly_keyed += 1,
                        Ok(()) => healed += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(
                                error = %e, entity_type = %entity_type, entity_id = %entity_id,
                                "key index backfill: upsert failed"
                            );
                        }
                    }
                }
                EntityLoadOutcome::Skip => skipped += 1,
                EntityLoadOutcome::Tombstoned => {
                    // ARN-238 healing pass: a deleted entity must not keep its
                    // declared keys. Emit a release marker per declared key so
                    // rows written before release-on-delete are purged.
                    let release_rows: Vec<temper_runtime::persistence::EntityKeyRow> = keys
                        .iter()
                        .map(|key| temper_runtime::persistence::EntityKeyRow {
                            key_name: key.name.clone(),
                            key_hash: String::new(),
                        })
                        .collect();
                    match store
                        .backfill_entity_keys(
                            tenant.as_str(),
                            entity_type,
                            entity_id,
                            &release_rows,
                        )
                        .await
                    {
                        Ok(()) => healed += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(
                                error = %e, entity_type = %entity_type, entity_id = %entity_id,
                                "key index backfill: tombstone release failed"
                            );
                        }
                    }
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

        // Watermark only if nothing failed — every existing entity was keyed or is
        // definitively skippable. Otherwise keyed misses keep scanning (sound), and a
        // later boot resumes from the remainder.
        if failed == 0 {
            state
                .mark_key_index_backfilled(tenant, entity_type, &current_key_set)
                .await;
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type, key_set = %current_key_set,
                total, newly_keyed, healed, already, skipped,
                "entity_key_index backfill complete; type watermarked"
            );
        } else {
            tracing::warn!(
                tenant = %tenant, entity_type = %entity_type,
                total, newly_keyed, healed, already, skipped, failed,
                "key index backfill: {failed} entities unresolved; type NOT watermarked (keyed misses keep scanning; will resume next run)"
            );
        }
    }
}
