//! Recovery and composite invariants (P11–P17).
#![allow(dead_code)]
#![allow(clippy::await_holding_lock)]

use std::collections::BTreeSet;

use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::PlatformStore;

use crate::harness::SimPlatformHarness;
use crate::invariants::event_store;

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

    let store = match event_store(harness) {
        Some(store) => store,
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
            if let Some(ids) = index.get(&expected_key)
                && !ids.contains(entity_id)
            {
                // Entity in event store but not in index — acceptable
                // (e.g. tombstoned). Not an isolation violation.
                continue;
            }

            // Cross-check: no other tenant should claim this persistence_id.
            for other_tenant in &tenant_ids {
                if other_tenant == tenant {
                    continue;
                }
                let cross_key = format!("{other_tenant}:{entity_type}");
                if let Some(ids) = index.get(&cross_key)
                    && ids.contains(entity_id)
                {
                    return Err(format!(
                        "P14: entity {entity_id} ({entity_type}) appears in both \
                         tenant '{tenant}' and tenant '{other_tenant}'"
                    ));
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

    let store = match event_store(harness) {
        Some(store) => store,
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

            if let Some(first) = events.first()
                && let Some(to_state) = first
                    .payload
                    .get("to_state")
                    .and_then(serde_json::Value::as_str)
                && to_state != expected_initial
            {
                return Err(format!(
                    "P15: entity {persistence_id} initial state is '{to_state}' \
                     but spec says '{expected_initial}'"
                ));
            }
        }
    }
    Ok(())
}

// ── P16: Event Replay Through TransitionTable ───────────────────────────

/// For each indexed entity, replays its event journal through the
/// `TransitionTable` and verifies that each event's `to_state` is a valid
/// transition from the previous state via the named action.
///
/// This is a **structural check**: it verifies that the TransitionTable
/// has a rule where the action name matches, the `from_states` contains
/// the current state, and the `to_state` matches the recorded `to_state`.
/// It does NOT re-evaluate guards (since `EvalContext` is not stored in
/// events) — the guard passed at dispatch time, which is sufficient.
pub async fn assert_p16_event_replay_fidelity(harness: &SimPlatformHarness) -> Result<(), String> {
    let index = harness.platform_state.server.entity_index.read().unwrap(); // ci-ok: infallible lock

    let store = match event_store(harness) {
        Some(store) => store,
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
            None => continue,
        };

        for entity_id in entity_ids {
            let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
            let events = store
                .read_events(&persistence_id, 0)
                .await
                .map_err(|e| format!("P16: read_events failed for {persistence_id}: {e}"))?;

            if events.is_empty() {
                continue;
            }

            let mut current_state = table.initial_state.clone();

            for event in &events {
                let action = event
                    .payload
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .or((!event.event_type.is_empty()).then_some(event.event_type.as_str()));

                let to_state = event
                    .payload
                    .get("to_state")
                    .and_then(serde_json::Value::as_str);

                // If event doesn't carry action/to_state, skip (metadata event).
                let (action, to_state) = match (action, to_state) {
                    (Some(a), Some(ts)) => (a, ts),
                    _ => continue,
                };

                // Verify the TransitionTable has a valid rule for this transition.
                let valid = table.rules.iter().any(|rule| {
                    if rule.name != action {
                        return false;
                    }
                    let state_ok = rule.from_states.is_empty()
                        || rule.from_states.iter().any(|s| s == &current_state);
                    if !state_ok {
                        return false;
                    }
                    match &rule.to_state {
                        Some(rule_to) => rule_to == to_state,
                        None => to_state == current_state, // self-loop
                    }
                });

                if !valid {
                    return Err(format!(
                        "P16: entity {persistence_id} seq {} has invalid transition: \
                         action='{action}', from='{current_state}', to='{to_state}' — \
                         no matching rule in TransitionTable",
                        event.sequence_nr
                    ));
                }

                current_state = to_state.to_string();
            }
        }
    }
    Ok(())
}

// ── P17: Spec Roundtrip Equivalence ─────────────────────────────────────

/// For each registered spec, rebuilds a `TransitionTable` from the stored
/// IOA source and verifies it is structurally equivalent to the in-registry
/// `TransitionTable`. Catches spec corruption during persistence or restore.
pub async fn assert_p17_spec_roundtrip_equivalence(
    harness: &SimPlatformHarness,
) -> Result<(), String> {
    let specs = harness
        .sim_platform_store
        .load_specs()
        .await
        .map_err(|e| format!("P17: failed to load specs: {e}"))?;

    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock

    for row in &specs {
        let tid = TenantId::new(&row.tenant);

        let registry_table = match registry.get_table(&tid, &row.entity_type) {
            Some(t) => t,
            None => continue, // P1 would catch this; skip here.
        };

        let rebuilt = match TransitionTable::try_from_ioa_source(&row.ioa_source) {
            Ok(t) => t,
            Err(e) => {
                return Err(format!(
                    "P17: failed to rebuild TransitionTable for ({}, {}): {e}",
                    row.tenant, row.entity_type
                ));
            }
        };

        // Compare initial state.
        if rebuilt.initial_state != registry_table.initial_state {
            return Err(format!(
                "P17: initial_state mismatch for ({}, {}): \
                 rebuilt='{}', registry='{}'",
                row.tenant, row.entity_type, rebuilt.initial_state, registry_table.initial_state
            ));
        }

        // Compare state sets.
        let mut rebuilt_states = rebuilt.states.clone();
        rebuilt_states.sort();
        let mut registry_states = registry_table.states.clone();
        registry_states.sort();
        if rebuilt_states != registry_states {
            return Err(format!(
                "P17: states mismatch for ({}, {}): \
                 rebuilt={rebuilt_states:?}, registry={registry_states:?}",
                row.tenant, row.entity_type
            ));
        }

        // Compare rule count.
        if rebuilt.rules.len() != registry_table.rules.len() {
            return Err(format!(
                "P17: rule count mismatch for ({}, {}): \
                 rebuilt={}, registry={}",
                row.tenant,
                row.entity_type,
                rebuilt.rules.len(),
                registry_table.rules.len()
            ));
        }

        // Compare each rule structurally.
        for (i, (r, reg)) in rebuilt
            .rules
            .iter()
            .zip(registry_table.rules.iter())
            .enumerate()
        {
            if r.name != reg.name {
                return Err(format!(
                    "P17: rule {i} name mismatch for ({}, {}): \
                     rebuilt='{}', registry='{}'",
                    row.tenant, row.entity_type, r.name, reg.name
                ));
            }
            let mut r_from = r.from_states.clone();
            r_from.sort();
            let mut reg_from = reg.from_states.clone();
            reg_from.sort();
            if r_from != reg_from {
                return Err(format!(
                    "P17: rule '{}'  from_states mismatch for ({}, {})",
                    r.name, row.tenant, row.entity_type
                ));
            }
            if r.to_state != reg.to_state {
                return Err(format!(
                    "P17: rule '{}' to_state mismatch for ({}, {}): \
                     rebuilt={:?}, registry={:?}",
                    r.name, row.tenant, row.entity_type, r.to_state, reg.to_state
                ));
            }
        }
    }
    Ok(())
}
