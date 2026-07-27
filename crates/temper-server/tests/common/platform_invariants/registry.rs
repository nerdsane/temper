//! Registry, store, index, tombstone, and Cedar invariants (P1-P7).

use super::*;

// ── P1: Registry-Store Consistency ──────────────────────────────────────

/// Every committed spec is either active in the registry or retained under an
/// explicit structured quarantine. Silent store/registry divergence is never
/// allowed.
pub async fn assert_p1_registry_store_consistency(
    harness: &SimPlatformHarness,
) -> Result<(), String> {
    let specs = harness
        .sim_platform_store
        .load_specs()
        .await
        .map_err(|e| format!("P1: failed to load specs: {e}"))?;
    let durable_quarantines = harness
        .sim_platform_store
        .load_registry_restore_quarantines()
        .await
        .map_err(|error| format!("P1: failed to load durable quarantines: {error}"))?;
    let constraint_versions = harness
        .sim_platform_store
        .load_tenant_constraints()
        .await
        .map_err(|error| format!("P1: failed to load tenant constraints: {error}"))?
        .into_iter()
        .map(|row| (row.tenant, row.version))
        .collect::<BTreeMap<_, _>>();
    let durable = durable_quarantines
        .iter()
        .map(|record| {
            (
                (record.tenant.clone(), record.entity_type.clone()),
                (record.spec_version, record.constraint_version),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let stored = specs
        .iter()
        .map(|row| {
            (
                (row.tenant.clone(), row.entity_type.clone()),
                (row.version, constraint_versions.get(&row.tenant).copied()),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let registry = harness.platform_state.registry.read().unwrap(); // ci-ok: infallible lock

    for row in &specs {
        let tid = TenantId::new(&row.tenant);
        if registry.get_table(&tid, &row.entity_type).is_none() {
            if !registry
                .restore_health()
                .is_quarantined(&row.tenant, &row.entity_type)
            {
                return Err(format!(
                    "P1: spec ({}, {}) is neither registered nor quarantined",
                    row.tenant, row.entity_type
                ));
            }
            let expected = (row.version, constraint_versions.get(&row.tenant).copied());
            if durable.get(&(row.tenant.clone(), row.entity_type.clone())) != Some(&expected) {
                return Err(format!(
                    "P1: spec ({}, {}) lacks a durable quarantine for source identity {:?}",
                    row.tenant, row.entity_type, expected
                ));
            }
            let process_identity = registry
                .restore_health()
                .quarantined_tenants
                .get(&row.tenant)
                .and_then(|tenant| tenant.entity_failures.get(&row.entity_type))
                .map(|failure| (failure.spec_version, failure.constraint_version));
            if process_identity != Some(expected) {
                return Err(format!(
                    "P1: process quarantine ({}, {}) has stale source identity",
                    row.tenant, row.entity_type
                ));
            }
        }
    }
    for record in &durable_quarantines {
        let key = (record.tenant.clone(), record.entity_type.clone());
        let identity = (record.spec_version, record.constraint_version);
        if stored.get(&key) != Some(&identity) {
            return Err(format!(
                "P1: active quarantine ({}, {}, {:?}) has no matching committed source snapshot",
                record.tenant, record.entity_type, identity
            ));
        }
        if !registry
            .restore_health()
            .is_quarantined(&record.tenant, &record.entity_type)
        {
            return Err(format!(
                "P1: durable quarantine ({}, {}) is absent from process health",
                record.tenant, record.entity_type
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

    let store = event_store(harness).ok_or_else(|| "P3: no event store configured".to_string())?;

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

    let store = event_store(harness).ok_or_else(|| "P4: no event store configured".to_string())?;

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
        let is_deleted = events.last().is_some_and(|last| {
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

    let store = event_store(harness).ok_or_else(|| "P5: no event store configured".to_string())?;

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

    for tenant in stored_map.keys() {
        if !in_memory.contains_key(tenant) {
            return Err(format!(
                "P7: tenant '{tenant}' has policy in store but not in memory"
            ));
        }
    }

    Ok(())
}
