//! Deterministic durable-quarantine operations for the simulation backend.

use super::*;

fn write_guard(inner: &mut SimPlatformStoreInner) -> Result<(), String> {
    let probability = inner.faults.quarantine_write_failure_prob;
    if inner.rng.chance(probability) {
        Err("SimPlatformStore: injected quarantine write failure".into())
    } else {
        Ok(())
    }
}

fn read_guard(inner: &mut SimPlatformStoreInner) -> Result<(), String> {
    let probability = inner.faults.quarantine_read_failure_prob;
    if inner.rng.chance(probability) {
        Err("SimPlatformStore: injected quarantine read failure".into())
    } else {
        Ok(())
    }
}

fn inject_registry_source_drift(inner: &mut SimPlatformStoreInner, tenant_scope: Option<&str>) {
    if inner.faults.registry_source_drift_budget == 0 {
        return;
    }
    inner.faults.registry_source_drift_budget -= 1;
    if let Some(row) = inner
        .specs
        .values_mut()
        .find(|row| row.committed && tenant_scope.is_none_or(|tenant| row.tenant == tenant))
    {
        row.version = row.version.saturating_add(1);
        row.updated_at = format!("sim-version-{}", row.version);
    }
}

fn record(
    row: RegistryQuarantineUpsert<'_>,
    existing: Option<&RegistryQuarantineRecord>,
) -> RegistryQuarantineRecord {
    RegistryQuarantineRecord {
        tenant: row.tenant.to_string(),
        entity_type: row.entity_type.to_string(),
        spec_version: row.spec_version,
        constraint_version: row.constraint_version,
        reason: row.reason.to_string(),
        source_kind: row.source_kind.to_string(),
        source_line: row.source_line,
        source_column: row.source_column,
        detail: row.detail.to_string(),
        acknowledged_at: existing.and_then(|record| record.acknowledged_at.clone()),
        created_at: existing.map_or_else(
            || "sim-created".to_string(),
            |record| record.created_at.clone(),
        ),
        last_observed_at: "sim-observed".to_string(),
    }
}

fn source_matches(
    inner: &SimPlatformStoreInner,
    tenant_scope: Option<&str>,
    expected: &RegistrySourceSnapshot,
) -> bool {
    let actual_specs = inner
        .specs
        .iter()
        .filter(|((tenant, _), row)| {
            row.committed && tenant_scope.is_none_or(|scope| tenant == scope)
        })
        .map(|(key, row)| (key.clone(), row.version))
        .collect::<BTreeMap<_, _>>();
    if actual_specs != expected.spec_versions {
        return false;
    }
    let actual_constraints = actual_specs
        .keys()
        .map(|(tenant, _)| tenant)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|tenant| {
            (
                tenant.clone(),
                inner.constraints.get(tenant).map(|(_, version)| *version),
            )
        })
        .collect::<BTreeMap<_, _>>();
    actual_constraints == expected.constraint_versions
}

pub(super) fn replace(
    store: &SimPlatformStore,
    tenant_scope: Option<&str>,
    source: &RegistrySourceSnapshot,
    active: &[RegistryQuarantineUpsert<'_>],
) -> Result<bool, String> {
    validate_registry_quarantine_snapshot(active)?;
    if active
        .iter()
        .any(|row| tenant_scope.is_some_and(|tenant| row.tenant != tenant))
    {
        return Err("tenant-scoped quarantine replacement received a foreign tenant".to_string());
    }
    let mut inner = store.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
    write_guard(&mut inner)?;
    inject_registry_source_drift(&mut inner, tenant_scope);
    if !source_matches(&inner, tenant_scope, source) {
        return Ok(false);
    }
    for ((row_tenant, _, _, _), entry) in &mut inner.registry_quarantines {
        if tenant_scope.is_none_or(|tenant| row_tenant == tenant) {
            entry.resolved = true;
        }
    }
    for row in active {
        let key = (
            row.tenant.to_string(),
            row.entity_type.to_string(),
            row.spec_version,
            row.constraint_version.unwrap_or(0),
        );
        let previous = inner
            .registry_quarantines
            .get(&key)
            .map(|entry| &entry.record);
        let reopened = record(*row, previous);
        inner.registry_quarantines.insert(
            key,
            SimRegistryQuarantineEntry {
                record: reopened,
                resolved: false,
            },
        );
    }
    Ok(true)
}

pub(super) fn resolve(
    store: &SimPlatformStore,
    source: &RegistrySourceSnapshot,
    resolutions: &[RegistryQuarantineResolution<'_>],
) -> Result<bool, String> {
    let mut inner = store.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
    write_guard(&mut inner)?;
    let Some(tenant) = resolutions.first().map(|resolution| resolution.tenant) else {
        return Err("registry quarantine resolution set must not be empty".to_string());
    };
    if resolutions
        .iter()
        .any(|resolution| resolution.tenant != tenant)
    {
        return Err("registry quarantine resolution set spans multiple tenants".to_string());
    }
    if !source_matches(&inner, Some(tenant), source) {
        return Ok(false);
    }
    let resolution_keys = resolutions
        .iter()
        .map(|resolution| {
            (
                resolution.tenant.to_string(),
                resolution.entity_type.to_string(),
                resolution.quarantined_version,
                resolution.quarantined_constraint_version.unwrap_or(0),
            )
        })
        .collect::<BTreeSet<_>>();
    let all_match = resolution_keys
        .iter()
        .all(|key| inner.registry_quarantines.contains_key(key));
    let uncovered_active = inner.registry_quarantines.iter().any(
        |((row_tenant, entity_type, version, constraint), entry)| {
            row_tenant == tenant
                && !entry.resolved
                && !resolution_keys.contains(&(
                    row_tenant.clone(),
                    entity_type.clone(),
                    *version,
                    *constraint,
                ))
        },
    );
    if !all_match || uncovered_active {
        return Ok(false);
    }
    for key in resolution_keys {
        if let Some(entry) = inner.registry_quarantines.get_mut(&key) {
            entry.resolved = true;
        }
    }
    Ok(true)
}

pub(super) fn acknowledge(
    store: &SimPlatformStore,
    tenant: &str,
    entity_type: &str,
    spec_version: i64,
    constraint_version: Option<i64>,
) -> Result<Option<(i64, Option<i64>)>, String> {
    let mut inner = store.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
    write_guard(&mut inner)?;
    let mut current = None;
    for ((row_tenant, row_type, row_version, row_constraint), entry) in
        &mut inner.registry_quarantines
    {
        if row_tenant == tenant && row_type == entity_type && !entry.resolved {
            current = Some((
                *row_version,
                (*row_constraint != 0).then_some(*row_constraint),
            ));
            if *row_version == spec_version && *row_constraint == constraint_version.unwrap_or(0) {
                entry.record.acknowledged_at = Some("sim-acknowledged".to_string());
            }
            break;
        }
    }
    Ok(current)
}

pub(super) fn load(
    store: &SimPlatformStore,
    tenant_scope: Option<&str>,
    limit: usize,
) -> Result<Vec<RegistryQuarantineRecord>, String> {
    let mut inner = store.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
    read_guard(&mut inner)?;
    Ok(inner
        .registry_quarantines
        .values()
        .filter(|entry| {
            !entry.resolved && tenant_scope.is_none_or(|tenant| entry.record.tenant == tenant)
        })
        .take(limit)
        .map(|entry| entry.record.clone())
        .collect())
}
