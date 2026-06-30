//! ADR-0153 declared-key backfill: key `entity_key_index` for pre-existing entities
//! and record the per-(tenant, entity_type) watermark, so a keyed read MISS can mean
//! authoritative absence (retiring #324's full-type scan — the 413, ARN-68).

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;

use crate::ServerState;
use crate::entity_actor::recover_entity_state_from_store;

use super::transition_table_for;

/// Outcome of loading one entity's current state for keying.
enum LoadOutcome {
    /// Loaded — key it from these fields.
    Fields(serde_json::Value),
    /// Definitively skippable: deleted, or a phantom with no events. Correctly NOT
    /// keyed, and NOT a failure (it must not block the watermark).
    Skip,
    /// The entity exists (it was enumerated from the durable store) but its current
    /// state could not be loaded — no transition table to replay with, an unreadable
    /// snapshot, or a replay error. Keying it is impossible, so the type must NOT be
    /// watermarked: otherwise a keyed miss for this present entity would wrongly read
    /// as authoritative absence. This is the soundness gate.
    LoadFailed,
}

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

        // Resumability: skip entities already keyed (avoids re-loading their state).
        let already_keyed: BTreeSet<String> = match store
            .keyed_entity_ids_for_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(ids) => ids.into_iter().collect(),
            Err(_) => BTreeSet::new(), // cannot resume → process all (correct, slower)
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
            match load_entity_fields(
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
                LoadOutcome::Fields(fields) => {
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
                LoadOutcome::Skip => skipped += 1,
                LoadOutcome::LoadFailed => {
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
            state.mark_key_index_backfilled(tenant, entity_type).await;
            tracing::info!(
                tenant = %tenant, entity_type = %entity_type,
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

/// Load one entity's current state for keying: snapshot if present and readable,
/// else event replay. Distinguishes "key it" from "definitively skip" from "exists
/// but unloadable" (the last fails the watermark — see [`LoadOutcome`]).
async fn load_entity_fields(
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    table: Option<&temper_jit::TransitionTable>,
    store: &crate::storage::BoxedEventStore,
    backend: crate::storage::BackendLabel,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> LoadOutcome {
    // Recover CURRENT state via snapshot + event-tail (so a field mutated after the
    // last snapshot — e.g. a Directory rename/move changing `Name`/`ParentId` — is
    // keyed at its current value, not a stale snapshot value). The STRICT path makes a
    // journal read failure propagate as `Err` instead of silently "starting fresh", so
    // we can tell "no events" apart from "could not read the journal". Without a
    // transition table we cannot replay → the entity is unresolved.
    let Some(table) = table else {
        return LoadOutcome::LoadFailed;
    };
    match recover_entity_state_from_store(
        tenant.as_str(),
        entity_type,
        entity_id,
        table,
        store,
        backend,
        &serde_json::json!({}),
        blob_store,
        true, // strict: a journal read failure → Err → LoadFailed (don't watermark)
    )
    .await
    {
        // Read failed (strict), replay budget exceeded, etc. — cannot key it, so the
        // type must NOT be watermarked.
        Err(_) => LoadOutcome::LoadFailed,
        Ok(state) if state.status == "Deleted" => LoadOutcome::Skip,
        // Strict guarantees the journal read succeeded, so zero events is a genuine
        // phantom (a catalog/field-index row with no journal) — unkeyable; safe to skip.
        Ok(state) if state.total_event_count == 0 => LoadOutcome::Skip,
        Ok(state) => LoadOutcome::Fields(state.fields),
    }
}
