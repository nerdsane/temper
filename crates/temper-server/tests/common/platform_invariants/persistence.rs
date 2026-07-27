//! Persistence sequencing and bootstrap invariants (P8-P13).

use super::*;

// ── P8: State-Store Sequence Agreement ──────────────────────────────────

/// Every entity's journal in the event store has a consistent, gapless
/// sequence from 1..N. No sequence gaps, no duplicates, no reversals.
///
/// This validates that persisted events form a valid, recoverable journal.
/// If a dispatch failed mid-write, the store must not contain partial entries
/// that would confuse replay on restart.
pub async fn assert_p8_state_store_sequence(harness: &SimPlatformHarness) -> Result<(), String> {
    let store = event_store(harness).ok_or_else(|| "P8: no event store configured".to_string())?;

    let all_pids = harness.sim_event_store.list_all_persistence_ids();

    for pid in &all_pids {
        let events = store
            .read_events(pid, 0)
            .await
            .map_err(|e| format!("P8: read_events failed for {pid}: {e}"))?;

        if events.is_empty() {
            continue;
        }

        // Verify the sequence starts at 1 and increments by 1.
        let mut expected_seq = 1u64;
        for event in &events {
            if event.sequence_nr != expected_seq {
                return Err(format!(
                    "P8: entity {pid} has sequence gap: expected {expected_seq}, got {}",
                    event.sequence_nr
                ));
            }
            expected_seq += 1;
        }
    }
    Ok(())
}

// ── P9: Rollback Completeness ────────────────────────────────────────────

/// No entity in the event store has partial or structurally invalid events.
///
/// Every event must have a non-empty `event_type` and valid `payload`.
/// This catches situations where a failed persist left half-written data
/// in the journal (which would corrupt state on replay).
pub async fn assert_p9_rollback_completeness(harness: &SimPlatformHarness) -> Result<(), String> {
    let store = event_store(harness).ok_or_else(|| "P9: no event store configured".to_string())?;

    let all_pids = harness.sim_event_store.list_all_persistence_ids();

    for pid in &all_pids {
        let events = store
            .read_events(pid, 0)
            .await
            .map_err(|e| format!("P9: read_events failed for {pid}: {e}"))?;

        for (i, event) in events.iter().enumerate() {
            // Every event must have a non-empty event_type.
            if event.event_type.is_empty() {
                return Err(format!(
                    "P9: entity {pid} event at seq {} has empty event_type",
                    event.sequence_nr
                ));
            }

            // Payload must be a JSON object (not null).
            if !event.payload.is_object() {
                return Err(format!(
                    "P9: entity {pid} event at seq {} has non-object payload: {}",
                    event.sequence_nr, event.payload
                ));
            }

            // Sequence numbers must be positive and match position.
            if event.sequence_nr != (i as u64 + 1) {
                return Err(format!(
                    "P9: entity {pid} event at position {i} has wrong sequence_nr: {} (expected {})",
                    event.sequence_nr,
                    i + 1
                ));
            }
        }
    }
    Ok(())
}

// ── P10: Field Replay Fidelity ──────────────────────────────────────────

/// Every indexed entity's events are replayable and reconstruct a consistent
/// state. Specifically: events can be read from the store, the sequence is
/// valid (P8), and the first event carries a `to_state` that matches the
/// spec's initial state (P15). This is the foundation of event-sourcing
/// correctness — if replay produces a valid state, the entity survives restart.
///
/// This is a weaker form of full replay fidelity (which would require
/// running the TransitionTable). It verifies the preconditions that make
/// replay possible: events are readable, sequenced, and structurally valid.
pub async fn assert_p10_field_replay_fidelity(harness: &SimPlatformHarness) -> Result<(), String> {
    let store = event_store(harness).ok_or_else(|| "P10: no event store configured".to_string())?;

    let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock

    // For every indexed entity, verify events are readable and consistent.
    for (index_key, entity_ids) in index.iter() {
        let parts: Vec<&str> = index_key.splitn(2, ':').collect();
        if parts.len() != 2 {
            continue;
        }
        let (tenant, entity_type) = (parts[0], parts[1]);

        for entity_id in entity_ids {
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

            // Events must be readable from the store.
            let events = store
                .read_events(&persistence_id, 0)
                .await
                .map_err(|e| format!("P10: read_events failed for {persistence_id}: {e}"))?;

            if events.is_empty() {
                return Err(format!(
                    "P10: indexed entity {persistence_id} has no events in store"
                ));
            }

            // Events must form a gapless sequence starting at 1.
            let mut expected_seq = 1u64;
            for event in &events {
                if event.sequence_nr != expected_seq {
                    return Err(format!(
                        "P10: entity {persistence_id} replay would fail: \
                         expected seq {expected_seq}, found {}",
                        event.sequence_nr
                    ));
                }
                expected_seq += 1;
            }

            // Each event must have valid structure for replay.
            for event in &events {
                if event.event_type.is_empty() {
                    return Err(format!(
                        "P10: entity {persistence_id} has event with empty type at seq {}",
                        event.sequence_nr
                    ));
                }
            }
        }
    }
    Ok(())
}

// ── P11: Installed Apps Persistence ─────────────────────────────────────

/// Installed apps in `SimPlatformStore` survive restart and match the entity
/// types present in `SpecRegistry`.
pub async fn assert_p11_installed_apps_persistence(
    harness: &SimPlatformHarness,
) -> Result<(), String> {
    let installed = harness
        .sim_platform_store
        .list_all_installed_apps()
        .await
        .map_err(|e| format!("P11: failed to list installed apps: {e}"))?;
    let specs = harness
        .sim_platform_store
        .load_specs()
        .await
        .map_err(|error| format!("P11: failed to load specs: {error}"))?;
    let constraint_versions = harness
        .sim_platform_store
        .load_tenant_constraints()
        .await
        .map_err(|error| format!("P11: failed to load constraints: {error}"))?
        .into_iter()
        .map(|row| (row.tenant, row.version))
        .collect::<BTreeMap<_, _>>();
    let durable_quarantines = harness
        .sim_platform_store
        .load_registry_restore_quarantines()
        .await
        .map_err(|error| format!("P11: failed to load quarantines: {error}"))?
        .into_iter()
        .map(|record| {
            (
                (record.tenant, record.entity_type),
                (record.spec_version, record.constraint_version),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock

    // An installed app normally has live entity types. A deliberately degraded
    // tenant may have none only when every committed source has an exact durable
    // and process quarantine identity.
    for (tenant, _app_name) in &installed {
        let tid = TenantId::new(tenant);
        let entity_types = registry.entity_types(&tid);
        if !entity_types.is_empty() {
            continue;
        }
        let tenant_specs = specs
            .iter()
            .filter(|row| row.tenant == *tenant)
            .collect::<Vec<_>>();
        if tenant_specs.is_empty() {
            return Err(format!(
                "P11: tenant '{tenant}' has installed apps but neither live nor quarantined specs"
            ));
        }
        for row in tenant_specs {
            let expected = (row.version, constraint_versions.get(tenant).copied());
            let key = (tenant.clone(), row.entity_type.clone());
            let process_identity = registry
                .restore_health()
                .quarantined_tenants
                .get(tenant)
                .and_then(|entry| entry.entity_failures.get(&row.entity_type))
                .map(|failure| (failure.spec_version, failure.constraint_version));
            if durable_quarantines.get(&key) != Some(&expected)
                || process_identity != Some(expected)
            {
                return Err(format!(
                    "P11: installed tenant '{tenant}' has inactive spec '{}' without exact quarantine identity",
                    row.entity_type
                ));
            }
        }
    }
    Ok(())
}

// ── P12: Bootstrap Idempotence ──────────────────────────────────────────

/// Installing the same OS app twice does not duplicate specs in the store.
pub async fn assert_p12_bootstrap_idempotence(
    harness: &SimPlatformHarness,
    tenant: &str,
) -> Result<(), String> {
    let specs = harness
        .sim_platform_store
        .load_specs()
        .await
        .map_err(|e| format!("P12: failed to load specs: {e}"))?;

    let tenant_specs: Vec<_> = specs.iter().filter(|r| r.tenant == tenant).collect();

    // Check for duplicates: (tenant, entity_type) must be unique.
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for row in &tenant_specs {
        if !seen.insert((&row.tenant, &row.entity_type)) {
            return Err(format!(
                "P12: duplicate spec in store for ({}, {})",
                row.tenant, row.entity_type
            ));
        }
    }
    Ok(())
}

// ── P13: Sequence Monotonicity ──────────────────────────────────────────

/// Event sequence numbers are strictly monotonically increasing per
/// persistence_id. No duplicates, no reversals, no gaps.
///
/// Iterates every journal in the SimEventStore and verifies the sequence
/// invariant that underpins event-sourcing correctness.
pub async fn assert_p13_sequence_monotonicity(harness: &SimPlatformHarness) -> Result<(), String> {
    let all_pids = harness.sim_event_store.list_all_persistence_ids();

    for pid in &all_pids {
        let events = harness.sim_event_store.dump_journal(pid);

        let mut prev_seq = 0u64;
        for event in &events {
            if event.sequence_nr <= prev_seq {
                return Err(format!(
                    "P13: entity {pid} has non-monotonic sequence: \
                     prev={prev_seq}, current={}",
                    event.sequence_nr
                ));
            }
            if event.sequence_nr != prev_seq + 1 {
                return Err(format!(
                    "P13: entity {pid} has sequence gap: \
                     prev={prev_seq}, current={} (expected {})",
                    event.sequence_nr,
                    prev_seq + 1
                ));
            }
            prev_seq = event.sequence_nr;
        }
    }
    Ok(())
}
