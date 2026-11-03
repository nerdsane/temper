//! Stored tenants read at boot and registered one at a time.
//!
//! A server with many tenants reads every tenant's spec rows before it
//! serves, in one read per store, but registers only its own tenants then:
//! registering converts and compiles every spec. Each other tenant's rows
//! wait as a [`StoredTenant`] until its restore registers them
//! ([`register_stored_tenant`]).

use std::collections::BTreeMap;

use temper_store_turso::{TursoEventStore, TursoSpecRow};

use super::{PersistedSpecRow, register_tenant_rows};
use crate::registry::SpecRegistry;

/// One stored tenant's spec rows and constraints, read but not registered.
pub struct StoredTenant {
    rows: StoredRows,
    constraints: Option<String>,
}

/// A tenant's spec rows as the store they came from returned them.
enum StoredRows {
    Postgres(Vec<PersistedSpecRow>),
    Turso(Vec<TursoSpecRow>),
}

/// Stored tenants by name.
pub type StoredTenants = BTreeMap<String, StoredTenant>;

/// Read every tenant's committed spec rows and constraints from Postgres,
/// after discarding the uncommitted versions failed installs left.
pub async fn read_stored_tenants_from_postgres(
    store: &temper_store_postgres::PostgresEventStore,
) -> Result<StoredTenants, String> {
    // Same rule as Turso: restore loads each type's committed version.
    match store.delete_uncommitted_specs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            "discarded {n} uncommitted spec versions during startup recovery; types with a committed version keep it"
        ),
        Err(e) => tracing::warn!("failed to discard uncommitted spec versions: {e}"),
    }
    let rows: Vec<PersistedSpecRow> = sqlx::query_as(&store.schema().qualify_sql(
        "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                levels_passed, levels_total, verification_result, updated_at \
         FROM {schema}specs WHERE committed = true \
         UNION ALL \
         SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                levels_passed, levels_total, verification_result, updated_at \
         FROM {schema}spec_committed_versions AS b WHERE NOT EXISTS ( \
             SELECT 1 FROM {schema}specs AS s \
             WHERE s.tenant = b.tenant AND s.entity_type = b.entity_type AND s.committed = true) \
         ORDER BY tenant, entity_type",
    ))
    .fetch_all(store.pool())
    .await
    .map_err(|e| format!("Failed to read specs from Postgres: {e}"))?;

    #[derive(sqlx::FromRow)]
    struct ConstraintRow {
        tenant: String,
        cross_invariants_toml: String,
    }

    let constraints_rows: Vec<ConstraintRow> = sqlx::query_as(&store.schema().qualify_sql(
        "SELECT tenant, cross_invariants_toml \
         FROM {schema}tenant_constraints \
         ORDER BY tenant",
    ))
    .fetch_all(store.pool())
    .await
    .map_err(|e| format!("Failed to read tenant constraints from Postgres: {e}"))?;

    let constraints_by_tenant: BTreeMap<String, String> = constraints_rows
        .into_iter()
        .map(|row| (row.tenant, row.cross_invariants_toml))
        .collect();

    Ok(group(
        rows,
        constraints_by_tenant,
        |row| row.tenant.clone(),
        StoredRows::Postgres,
    ))
}

/// Read every tenant's committed spec rows and constraints from Turso,
/// after discarding the uncommitted versions failed installs left.
pub async fn read_stored_tenants_from_turso(
    turso: &TursoEventStore,
) -> Result<StoredTenants, String> {
    // Discard uncommitted spec versions left by failed or interrupted installs.
    match turso.delete_uncommitted_specs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            "discarded {n} uncommitted spec versions during startup recovery; types with a committed version keep it"
        ),
        Err(e) => tracing::warn!("failed to discard uncommitted spec versions: {e}"),
    }
    let rows = turso
        .load_specs()
        .await
        .map_err(|e| format!("Failed to read specs from Turso: {e}"))?;
    let constraints_rows = turso
        .load_tenant_constraints()
        .await
        .map_err(|e| format!("Failed to read tenant constraints from Turso: {e}"))?;

    let constraints_by_tenant: BTreeMap<String, String> = constraints_rows
        .into_iter()
        .map(|row| (row.tenant, row.cross_invariants_toml))
        .collect();

    Ok(group(
        rows,
        constraints_by_tenant,
        |row| row.tenant.clone(),
        StoredRows::Turso,
    ))
}

/// Group `rows` by tenant, each with its constraints.
fn group<R>(
    rows: Vec<R>,
    mut constraints_by_tenant: BTreeMap<String, String>,
    tenant_of: impl Fn(&R) -> String,
    stored_rows: impl Fn(Vec<R>) -> StoredRows,
) -> StoredTenants {
    let mut grouped: BTreeMap<String, Vec<R>> = BTreeMap::new();
    for row in rows {
        grouped.entry(tenant_of(&row)).or_default().push(row);
    }
    grouped
        .into_iter()
        .map(|(tenant, rows)| {
            let constraints = constraints_by_tenant.remove(&tenant);
            let stored = StoredTenant {
                rows: stored_rows(rows),
                constraints,
            };
            (tenant, stored)
        })
        .collect()
}

/// Register `stored`, the rows read for `tenant`, into `registry`, and
/// return how many specs it restored. A tenant whose rows do not load is
/// marked unavailable, as at boot.
pub fn register_stored_tenant(
    registry: &mut SpecRegistry,
    tenant: String,
    stored: StoredTenant,
) -> usize {
    let StoredTenant { rows, constraints } = stored;
    match rows {
        StoredRows::Postgres(rows) => register_tenant_rows(
            registry,
            tenant,
            rows,
            constraints,
            &|row| row.csdl_xml.clone(),
            &|row| (row.entity_type.clone(), row.ioa_source.clone()),
        ),
        StoredRows::Turso(rows) => register_tenant_rows(
            registry,
            tenant,
            rows,
            constraints,
            &|row| row.csdl_xml.clone(),
            &|row| (row.entity_type.clone(), row.ioa_source.clone()),
        ),
    }
}
