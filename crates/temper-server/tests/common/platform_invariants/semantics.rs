//! Tenant and specification semantic invariants (P14-P17).

use super::*;

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
    let stored_constraints = harness
        .sim_platform_store
        .load_tenant_constraints()
        .await
        .map_err(|error| format!("P17: failed to load constraints: {error}"))?
        .into_iter()
        .map(|row| (row.tenant, row.source))
        .collect::<BTreeMap<_, _>>();

    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock

    for row in &specs {
        let tid = TenantId::new(&row.tenant);

        let registry_table = match registry.get_table(&tid, &row.entity_type) {
            Some(t) => t,
            None => continue, // P1 requires an explicit quarantine for this row.
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

    for tenant_id in registry.tenant_ids() {
        let stored_source = stored_constraints
            .get(tenant_id.as_str())
            .map(String::as_str);
        let registry_source = registry
            .get_tenant(tenant_id)
            .and_then(|tenant| tenant.cross_invariants_source.as_deref());
        if registry_source != stored_source {
            return Err(format!(
                "P17: cross-invariant source mismatch for tenant '{}': stored={stored_source:?}, registry={registry_source:?}",
                tenant_id.as_str()
            ));
        }
    }
    Ok(())
}

// ── Composite checks ────────────────────────────────────────────────────

/// Check invariants that must hold even mid-operation under fault injection.
///
/// P1/P2 may be transiently violated while a faulty `install_os_app` leaves
/// uncommitted rows. Startup removes those WAL-style partial writes. Committed
/// invalid rows are retained and must appear in structured restore quarantine
/// health. Mid-operation checks therefore cover only invariants that cannot be
/// transiently violated by install rollback.
pub async fn assert_mid_operation_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p8_state_store_sequence(harness).await?;
    assert_p9_rollback_completeness(harness).await?;
    assert_p13_sequence_monotonicity(harness).await?;
    Ok(())
}

/// Check all boot-cycle invariants (P1, P2, P6, P7, P11, P17).
///
/// These invariants should hold after every restart: the in-memory state
/// is consistent with the durable stores.
pub async fn assert_boot_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p1_registry_store_consistency(harness).await?;
    assert_p2_store_registry_consistency(harness).await?;
    assert_p6_cedar_spec_coherence(harness).await?;
    assert_p7_cedar_persistence(harness).await?;
    assert_p11_installed_apps_persistence(harness).await?;
    assert_p17_spec_roundtrip_equivalence(harness).await?;
    Ok(())
}

/// Check all data-plane invariants (P3, P4, P5, P8, P9, P10, P13, P14, P15, P16).
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
    assert_p16_event_replay_fidelity(harness).await?;
    Ok(())
}
