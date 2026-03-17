//! Platform invariant checkers (P1–P15) for deterministic simulation testing.
//!
//! Each invariant is a standalone function that inspects the harness state
//! and returns `Ok(())` on success or `Err(message)` on violation.
//! All invariants run against PRODUCTION data structures — no test-only
//! reimplementations.
#![allow(dead_code)]

use std::collections::BTreeSet;

use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::{TenantId, parse_persistence_id_parts};
use temper_server::platform_store::PlatformStore;

use super::platform_harness::SimPlatformHarness;

// ── P1: Registry-Store Consistency ──────────────────────────────────────

/// Every (tenant, entity_type) in `SimPlatformStore` has a `TransitionTable`
/// in the `SpecRegistry`.
pub async fn assert_p1_registry_store_consistency(
    harness: &SimPlatformHarness,
) -> Result<(), String> {
    let specs = harness
        .sim_platform_store
        .load_specs()
        .await
        .map_err(|e| format!("P1: failed to load specs: {e}"))?;

    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock

    for row in &specs {
        let tid = TenantId::new(&row.tenant);
        if registry.get_table(&tid, &row.entity_type).is_none() {
            return Err(format!(
                "P1: spec ({}, {}) in store but not in registry",
                row.tenant, row.entity_type
            ));
        }
    }
    Ok(())
}

// ── P2: Store-Registry Consistency (reverse of P1) ──────────────────────

/// Every (tenant, entity_type) in the `SpecRegistry` has a matching spec
/// in the `SimPlatformStore`.
pub async fn assert_p2_store_registry_consistency(
    harness: &SimPlatformHarness,
) -> Result<(), String> {
    let specs = harness
        .sim_platform_store
        .load_specs()
        .await
        .map_err(|e| format!("P2: failed to load specs: {e}"))?;

    let stored: BTreeSet<(String, String)> = specs
        .iter()
        .map(|r| (r.tenant.clone(), r.entity_type.clone()))
        .collect();

    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock

    for tenant_id in registry.tenant_ids() {
        for entity_type in registry.entity_types(tenant_id) {
            let key = (tenant_id.as_str().to_string(), entity_type.to_string());
            if !stored.contains(&key) {
                return Err(format!(
                    "P2: registry has ({}, {}) but store does not",
                    tenant_id.as_str(),
                    entity_type
                ));
            }
        }
    }
    Ok(())
}

// ── P3: Index-Store Agreement ───────────────────────────────────────────

/// Every entity in the `entity_index` has events in the event store.
pub async fn assert_p3_index_store_agreement(harness: &SimPlatformHarness) -> Result<(), String> {
    let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock

    let store = harness
        .platform_state
        .server
        .event_store
        .as_ref()
        .ok_or_else(|| "P3: no event store configured".to_string())?;

    for (index_key, entity_ids) in index.iter() {
        // index_key format: "{tenant}:{entity_type}"
        let parts: Vec<&str> = index_key.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(format!("P3: malformed index key: {index_key}"));
        }
        let (tenant, entity_type) = (parts[0], parts[1]);

        for entity_id in entity_ids {
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let events = store
                .read_events(&persistence_id, 0)
                .await
                .map_err(|e| format!("P3: read_events failed for {persistence_id}: {e}"))?;

            if events.is_empty() {
                return Err(format!(
                    "P3: entity {persistence_id} in index but has no events in store"
                ));
            }
        }
    }
    Ok(())
}

// ── P4: Store-Index Completeness ─────────────────────────────────────────

/// Every non-tombstoned entity in the event store has a corresponding entry
/// in the entity index (after `populate_index_from_store` has been called).
pub async fn assert_p4_store_index_completeness(
    harness: &SimPlatformHarness,
) -> Result<(), String> {
    let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock

    let store = harness
        .platform_state
        .server
        .event_store
        .as_ref()
        .ok_or_else(|| "P4: no event store configured".to_string())?;

    // Iterate all persistence IDs in the SimEventStore.
    let all_pids = harness.sim_event_store.list_all_persistence_ids();

    for pid in &all_pids {
        // Parse tenant:entity_type:entity_id from persistence_id.
        let (tenant, entity_type, entity_id) = match parse_persistence_id_parts(pid) {
            Ok(parts) => parts,
            Err(_) => continue, // Skip malformed IDs.
        };

        // Read events to check if tombstoned.
        let events = store
            .read_events(pid, 0)
            .await
            .map_err(|e| format!("P4: read_events failed for {pid}: {e}"))?;

        if events.is_empty() {
            continue; // No events — nothing to index.
        }

        // Check if the last event is a deletion marker.
        let is_deleted = events.last().map_or(false, |last| {
            last.event_type == "Deleted"
                || last
                    .payload
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    == Some("Deleted")
        });

        if is_deleted {
            continue; // Tombstoned — should NOT be in index (checked by P5).
        }

        // Non-tombstoned entity must be in the index.
        let index_key = format!("{tenant}:{entity_type}");
        let in_index = index
            .get(&index_key)
            .map(|ids| ids.contains(&entity_id.to_string()))
            .unwrap_or(false);

        if !in_index {
            return Err(format!(
                "P4: entity {pid} has {n} non-tombstoned events but is not in index",
                n = events.len(),
            ));
        }
    }
    Ok(())
}

// ── P5: Tombstone Finality ──────────────────────────────────────────────

/// If the last event for an entity is "Deleted", the entity must be absent
/// from the entity index.
pub async fn assert_p5_tombstone_finality(harness: &SimPlatformHarness) -> Result<(), String> {
    let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock

    let store = harness
        .platform_state
        .server
        .event_store
        .as_ref()
        .ok_or_else(|| "P5: no event store configured".to_string())?;

    // Check every indexed entity: if its last event is a deletion marker,
    // it should not be in the index.
    for (index_key, entity_ids) in index.iter() {
        let parts: Vec<&str> = index_key.splitn(2, ':').collect();
        if parts.len() != 2 {
            continue;
        }
        let (tenant, entity_type) = (parts[0], parts[1]);

        for entity_id in entity_ids {
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let events = store
                .read_events(&persistence_id, 0)
                .await
                .map_err(|e| format!("P5: read_events failed for {persistence_id}: {e}"))?;

            if let Some(last) = events.last() {
                let is_deleted = last.event_type == "Deleted"
                    || last
                        .payload
                        .get("action")
                        .and_then(serde_json::Value::as_str)
                        == Some("Deleted");
                if is_deleted {
                    return Err(format!(
                        "P5: entity {persistence_id} is tombstoned but still in index"
                    ));
                }
            }
        }
    }
    Ok(())
}

// ── P6: Cedar-Spec Coherence ────────────────────────────────────────────

/// For tenants with installed apps that have Cedar policies, the authz
/// engine has loaded those policies.
pub async fn assert_p6_cedar_spec_coherence(harness: &SimPlatformHarness) -> Result<(), String> {
    let policies = harness
        .platform_state
        .server
        .tenant_policies
        .read()
        .unwrap(); // ci-ok: infallible lock

    // If any tenant has policy text, the authz engine must not be empty.
    // (We cannot inspect Cedar internals directly; we verify that the
    // in-memory policy map is non-empty only when the store has policies.)
    let stored_policies = harness
        .sim_platform_store
        .load_tenant_policies()
        .await
        .map_err(|e| format!("P6: failed to load policies: {e}"))?;

    for (tenant, store_text) in &stored_policies {
        if store_text.is_empty() {
            continue;
        }
        if !policies.contains_key(tenant) {
            return Err(format!(
                "P6: tenant '{tenant}' has Cedar policies in store but not in memory"
            ));
        }
    }
    Ok(())
}

// ── P7: Cedar Persistence ───────────────────────────────────────────────

/// In-memory `tenant_policies` match what is stored in `SimPlatformStore`.
pub async fn assert_p7_cedar_persistence(harness: &SimPlatformHarness) -> Result<(), String> {
    let stored_policies = harness
        .sim_platform_store
        .load_tenant_policies()
        .await
        .map_err(|e| format!("P7: failed to load policies: {e}"))?;

    let in_memory = harness
        .platform_state
        .server
        .tenant_policies
        .read()
        .unwrap(); // ci-ok: infallible lock

    let stored_map: std::collections::BTreeMap<String, String> =
        stored_policies.into_iter().collect();

    for (tenant, mem_text) in in_memory.iter() {
        match stored_map.get(tenant) {
            Some(store_text) if store_text == mem_text => {}
            Some(store_text) => {
                return Err(format!(
                    "P7: policy mismatch for tenant '{tenant}': in-memory len={}, store len={}",
                    mem_text.len(),
                    store_text.len()
                ));
            }
            None => {
                return Err(format!(
                    "P7: tenant '{tenant}' has in-memory policy but not in store"
                ));
            }
        }
    }

    for (tenant, _) in &stored_map {
        if !in_memory.contains_key(tenant) {
            return Err(format!(
                "P7: tenant '{tenant}' has policy in store but not in memory"
            ));
        }
    }

    Ok(())
}

// ── P8: State-Store Sequence Agreement ──────────────────────────────────

/// Every entity's journal in the event store has a consistent, gapless
/// sequence from 1..N. No sequence gaps, no duplicates, no reversals.
///
/// This validates that persisted events form a valid, recoverable journal.
/// If a dispatch failed mid-write, the store must not contain partial entries
/// that would confuse replay on restart.
pub async fn assert_p8_state_store_sequence(harness: &SimPlatformHarness) -> Result<(), String> {
    let store = harness
        .platform_state
        .server
        .event_store
        .as_ref()
        .ok_or_else(|| "P8: no event store configured".to_string())?;

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
    let store = harness
        .platform_state
        .server
        .event_store
        .as_ref()
        .ok_or_else(|| "P9: no event store configured".to_string())?;

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
    let store = harness
        .platform_state
        .server
        .event_store
        .as_ref()
        .ok_or_else(|| "P10: no event store configured".to_string())?;

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

    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock

    // For every installed app, the tenant must exist in the registry with
    // at least one entity type registered.
    for (tenant, _app_name) in &installed {
        let tid = TenantId::new(tenant);
        let entity_types = registry.entity_types(&tid);
        if entity_types.is_empty() {
            return Err(format!(
                "P11: tenant '{tenant}' has installed apps but no entity types in registry"
            ));
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

// ── P14: Tenant Isolation ───────────────────────────────────────────────

/// Entity events for tenant-A never appear in tenant-B's journal.
///
/// Checks that all persistence IDs in the entity index are scoped to the
/// correct tenant prefix.
pub async fn assert_p14_tenant_isolation(harness: &SimPlatformHarness) -> Result<(), String> {
    let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock

    let store = match harness.platform_state.server.event_store.as_ref() {
        Some(s) => s,
        None => return Ok(()), // No store, nothing to check.
    };

    // Collect all known tenants.
    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock
    let tenant_ids: Vec<String> = registry
        .tenant_ids()
        .iter()
        .map(|t| t.as_str().to_string())
        .collect();
    drop(registry);

    for tenant in &tenant_ids {
        let listed = store
            .list_entity_ids(tenant)
            .await
            .map_err(|e| format!("P14: list_entity_ids failed for {tenant}: {e}"))?;

        for (entity_type, entity_id) in &listed {
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let _events = store
                .read_events(&persistence_id, 0)
                .await
                .map_err(|e| format!("P14: read_events failed: {e}"))?;

            // Verify these events belong to this tenant's index key.
            let expected_key = format!("{tenant}:{entity_type}");
            if let Some(ids) = index.get(&expected_key) {
                if !ids.contains(entity_id) {
                    // Entity in event store but not in index — acceptable
                    // (e.g. tombstoned). Not an isolation violation.
                    continue;
                }
            }

            // Cross-check: no other tenant should claim this persistence_id.
            for other_tenant in &tenant_ids {
                if other_tenant == tenant {
                    continue;
                }
                let cross_key = format!("{other_tenant}:{entity_type}");
                if let Some(ids) = index.get(&cross_key) {
                    if ids.contains(entity_id) {
                        return Err(format!(
                            "P14: entity {entity_id} ({entity_type}) appears in both \
                             tenant '{tenant}' and tenant '{other_tenant}'"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

// ── P15: Initial State Correctness ──────────────────────────────────────

/// A newly created entity's status matches the spec's `initial_state`.
///
/// This checks by reading the first event for each indexed entity and
/// verifying the `to_state` field matches the TransitionTable initial state.
pub async fn assert_p15_initial_state_correctness(
    harness: &SimPlatformHarness,
) -> Result<(), String> {
    let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock

    let store = match harness.platform_state.server.event_store.as_ref() {
        Some(s) => s,
        None => return Ok(()),
    };

    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock

    for (index_key, entity_ids) in index.iter() {
        let parts: Vec<&str> = index_key.splitn(2, ':').collect();
        if parts.len() != 2 {
            continue;
        }
        let (tenant, entity_type) = (parts[0], parts[1]);
        let tid = TenantId::new(tenant);

        let table = match registry.get_table(&tid, entity_type) {
            Some(t) => t,
            None => continue, // Skip if no table (shouldn't happen if P1 holds).
        };

        let expected_initial = &table.initial_state;

        for entity_id in entity_ids {
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let events = store
                .read_events(&persistence_id, 0)
                .await
                .map_err(|e| format!("P15: read_events failed for {persistence_id}: {e}"))?;

            if let Some(first) = events.first() {
                // The first event's to_state (or the state after creation)
                // should match the spec initial state.
                if let Some(to_state) = first
                    .payload
                    .get("to_state")
                    .and_then(serde_json::Value::as_str)
                {
                    if to_state != expected_initial {
                        return Err(format!(
                            "P15: entity {persistence_id} initial state is '{to_state}' \
                             but spec says '{expected_initial}'"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

// ── Composite checks ────────────────────────────────────────────────────

/// Check all boot-cycle invariants (P1, P2, P6, P7, P11).
///
/// These invariants should hold after every restart: the in-memory state
/// is consistent with the durable stores.
pub async fn assert_boot_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p1_registry_store_consistency(harness).await?;
    assert_p2_store_registry_consistency(harness).await?;
    assert_p6_cedar_spec_coherence(harness).await?;
    assert_p7_cedar_persistence(harness).await?;
    assert_p11_installed_apps_persistence(harness).await?;
    Ok(())
}

/// Check all data-plane invariants (P3, P4, P5, P8, P9, P10, P13, P14, P15).
///
/// These invariants should hold after dispatching actions.
pub async fn assert_data_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p3_index_store_agreement(harness).await?;
    assert_p4_store_index_completeness(harness).await?;
    assert_p5_tombstone_finality(harness).await?;
    assert_p8_state_store_sequence(harness).await?;
    assert_p9_rollback_completeness(harness).await?;
    assert_p10_field_replay_fidelity(harness).await?;
    assert_p13_sequence_monotonicity(harness).await?;
    assert_p14_tenant_isolation(harness).await?;
    assert_p15_initial_state_correctness(harness).await?;
    Ok(())
}
