//! Durable registry-restore quarantine persistence.

use std::collections::{BTreeMap, BTreeSet};

use libsql::{Transaction, TransactionBehavior, params};
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;

/// Active quarantine row returned to the server repair API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TursoRegistryQuarantineRow {
    /// Tenant owning the persisted spec.
    pub tenant: String,
    /// Entity type withheld from activation.
    pub entity_type: String,
    /// Persisted source version that failed.
    pub spec_version: i64,
    /// Constraint source version compiled with the spec, or `None` if absent.
    pub constraint_version: Option<i64>,
    /// Stable failure category.
    pub reason: String,
    /// Source document category.
    pub source_kind: String,
    /// One-based source line, when available.
    pub source_line: Option<i64>,
    /// One-based source column, when available.
    pub source_column: Option<i64>,
    /// Bounded parser/registration diagnostic.
    pub detail: String,
    /// ISO-8601 acknowledgment timestamp.
    pub acknowledged_at: Option<String>,
    /// ISO-8601 first-observed timestamp.
    pub created_at: String,
    /// ISO-8601 most-recent-observation timestamp.
    pub last_observed_at: String,
}

/// Versioned quarantine payload produced by one restore attempt.
#[derive(Debug, Clone, Copy)]
pub struct TursoRegistryQuarantineUpsert<'a> {
    /// Tenant owning the persisted spec.
    pub tenant: &'a str,
    /// Entity type withheld from activation.
    pub entity_type: &'a str,
    /// Persisted source version that failed.
    pub spec_version: i64,
    /// Constraint source version compiled with the spec, or `None` if absent.
    pub constraint_version: Option<i64>,
    /// Stable failure category.
    pub reason: &'a str,
    /// Source document category.
    pub source_kind: &'a str,
    /// One-based source line, when available.
    pub source_line: Option<i64>,
    /// One-based source column, when available.
    pub source_column: Option<i64>,
    /// Bounded parser/registration diagnostic.
    pub detail: &'a str,
}

/// Exact active record and validated replacement version for atomic resolution.
#[derive(Debug, Clone, Copy)]
pub struct TursoRegistryQuarantineResolution<'a> {
    /// Tenant owning both records.
    pub tenant: &'a str,
    /// Entity type being reactivated.
    pub entity_type: &'a str,
    /// Active quarantine version to resolve.
    pub quarantined_version: i64,
    /// Constraint version recorded on the active quarantine, or `None` if absent.
    pub quarantined_constraint_version: Option<i64>,
}

/// Complete committed source manifest for one quarantine transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TursoRegistrySourceSnapshot {
    /// Exact committed spec versions in scope.
    pub spec_versions: BTreeMap<(String, String), i64>,
    /// Exact constraint presence/version for every tenant in scope.
    pub constraint_versions: BTreeMap<String, Option<i64>>,
}

async fn source_snapshot_matches(
    transaction: &Transaction,
    tenant_scope: Option<&str>,
    expected: &TursoRegistrySourceSnapshot,
) -> Result<bool, PersistenceError> {
    let mut spec_rows = match tenant_scope {
        Some(tenant) => {
            transaction
                .query(
                    "SELECT tenant, entity_type, version FROM specs \
                     WHERE committed = 1 AND tenant = ?1 ORDER BY tenant, entity_type",
                    params![tenant],
                )
                .await
        }
        None => {
            transaction
                .query(
                    "SELECT tenant, entity_type, version FROM specs \
                     WHERE committed = 1 ORDER BY tenant, entity_type",
                    (),
                )
                .await
        }
    }
    .map_err(storage_error)?;
    let mut actual_specs = BTreeMap::new();
    while let Some(row) = spec_rows.next().await.map_err(storage_error)? {
        actual_specs.insert(
            (
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ),
            row.get::<i64>(2).map_err(storage_error)?,
        );
    }
    drop(spec_rows);
    if actual_specs != expected.spec_versions {
        return Ok(false);
    }

    let mut constraint_rows = match tenant_scope {
        Some(tenant) => {
            transaction
                .query(
                    "SELECT tenant, version FROM tenant_constraints \
                     WHERE tenant = ?1 ORDER BY tenant",
                    params![tenant],
                )
                .await
        }
        None => {
            transaction
                .query(
                    "SELECT tenant, version FROM tenant_constraints ORDER BY tenant",
                    (),
                )
                .await
        }
    }
    .map_err(storage_error)?;
    let mut persisted = BTreeMap::new();
    while let Some(row) = constraint_rows.next().await.map_err(storage_error)? {
        persisted.insert(
            row.get::<String>(0).map_err(storage_error)?,
            row.get::<i64>(1).map_err(storage_error)?,
        );
    }
    let tenants = actual_specs
        .keys()
        .map(|(tenant, _)| tenant)
        .collect::<BTreeSet<_>>();
    let actual_constraints = tenants
        .into_iter()
        .map(|tenant| (tenant.clone(), persisted.get(tenant).copied()))
        .collect::<BTreeMap<_, _>>();
    Ok(actual_constraints == expected.constraint_versions)
}

fn single_resolution_tenant<'a>(
    resolutions: &'a [TursoRegistryQuarantineResolution<'a>],
) -> Result<&'a str, PersistenceError> {
    let Some(first) = resolutions.first() else {
        return Err(PersistenceError::Storage(
            "registry quarantine resolution set must not be empty".to_string(),
        ));
    };
    if resolutions
        .iter()
        .any(|resolution| resolution.tenant != first.tenant)
    {
        return Err(PersistenceError::Storage(
            "registry quarantine resolution set spans multiple tenants".to_string(),
        ));
    }
    Ok(first.tenant)
}

async fn exact_quarantine_exists(
    transaction: &Transaction,
    resolution: &TursoRegistryQuarantineResolution<'_>,
) -> Result<bool, PersistenceError> {
    let mut rows = transaction
        .query(
            "SELECT 1 FROM registry_restore_quarantines \
             WHERE tenant = ?1 AND entity_type = ?2 AND spec_version = ?3 \
               AND constraint_version = ?4 LIMIT 1",
            params![
                resolution.tenant,
                resolution.entity_type,
                resolution.quarantined_version,
                resolution.quarantined_constraint_version.unwrap_or(0)
            ],
        )
        .await
        .map_err(storage_error)?;
    let exists = rows.next().await.map_err(storage_error)?.is_some();
    drop(rows);
    Ok(exists)
}

impl TursoEventStore {
    /// Replace the active restore-quarantine snapshot while retaining resolved history.
    #[instrument(skip_all, fields(otel.name = "turso.replace_registry_restore_quarantines"))]
    pub async fn replace_registry_restore_quarantines(
        &self,
        source: &TursoRegistrySourceSnapshot,
        active: &[TursoRegistryQuarantineUpsert<'_>],
    ) -> Result<bool, PersistenceError> {
        self.replace_registry_restore_quarantines_scoped(None, source, active)
            .await
    }

    /// Replace one tenant's active snapshot without changing other tenants.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.replace_registry_restore_quarantines_for_tenant"))]
    pub async fn replace_registry_restore_quarantines_for_tenant(
        &self,
        tenant: &str,
        source: &TursoRegistrySourceSnapshot,
        active: &[TursoRegistryQuarantineUpsert<'_>],
    ) -> Result<bool, PersistenceError> {
        if active.iter().any(|row| row.tenant != tenant) {
            return Err(PersistenceError::Storage(
                "tenant-scoped quarantine replacement received a foreign tenant".to_string(),
            ));
        }
        self.replace_registry_restore_quarantines_scoped(Some(tenant), source, active)
            .await
    }

    async fn replace_registry_restore_quarantines_scoped(
        &self,
        tenant: Option<&str>,
        source: &TursoRegistrySourceSnapshot,
        active: &[TursoRegistryQuarantineUpsert<'_>],
    ) -> Result<bool, PersistenceError> {
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        if !source_snapshot_matches(&tx, tenant, source).await? {
            tx.rollback().await.map_err(storage_error)?;
            return Ok(false);
        }
        match tenant {
            Some(tenant) => {
                tx.execute(
                    "UPDATE registry_restore_quarantines SET resolved_at = datetime('now') \
                     WHERE tenant = ?1 AND resolved_at IS NULL",
                    params![tenant],
                )
                .await
                .map_err(storage_error)?;
            }
            None => {
                tx.execute(
                    "UPDATE registry_restore_quarantines SET resolved_at = datetime('now') \
                     WHERE resolved_at IS NULL",
                    (),
                )
                .await
                .map_err(storage_error)?;
            }
        }
        for row in active {
            let changed = tx.execute(
                "INSERT INTO registry_restore_quarantines \
                 (tenant, entity_type, spec_version, constraint_version, reason, source_kind, source_line, \
                  source_column, detail, created_at, last_observed_at, resolved_at) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'), datetime('now'), NULL \
                 WHERE EXISTS (SELECT 1 FROM specs \
                               WHERE specs.tenant = ?1 AND specs.entity_type = ?2 \
                                 AND specs.version = ?3 AND specs.committed = 1) \
                   AND ((?4 = 0 AND NOT EXISTS (SELECT 1 FROM tenant_constraints \
                                                WHERE tenant_constraints.tenant = ?1)) \
                        OR (?4 > 0 AND EXISTS (SELECT 1 FROM tenant_constraints \
                                              WHERE tenant_constraints.tenant = ?1 \
                                                AND tenant_constraints.version = ?4))) \
                 ON CONFLICT (tenant, entity_type, spec_version, constraint_version) DO UPDATE SET \
                    reason = excluded.reason, source_kind = excluded.source_kind, \
                    source_line = excluded.source_line, source_column = excluded.source_column, \
                    detail = excluded.detail, last_observed_at = datetime('now'), resolved_at = NULL",
                params![
                    row.tenant,
                    row.entity_type,
                    row.spec_version,
                    row.constraint_version.unwrap_or(0),
                    row.reason,
                    row.source_kind,
                    row.source_line,
                    row.source_column,
                    row.detail
                ],
            )
            .await
            .map_err(storage_error)?;
            if changed != 1 {
                tx.rollback().await.map_err(storage_error)?;
                return Ok(false);
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    /// Atomically resolve an exact set of active records after repair validation.
    #[instrument(skip_all, fields(otel.name = "turso.resolve_registry_restore_quarantines"))]
    pub async fn resolve_registry_restore_quarantines(
        &self,
        source: &TursoRegistrySourceSnapshot,
        resolutions: &[TursoRegistryQuarantineResolution<'_>],
    ) -> Result<bool, PersistenceError> {
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let tenant = single_resolution_tenant(resolutions)?;
        if !source_snapshot_matches(&tx, Some(tenant), source).await? {
            tx.rollback().await.map_err(storage_error)?;
            return Ok(false);
        }
        for resolution in resolutions {
            let changed = tx
                .execute(
                    "UPDATE registry_restore_quarantines SET resolved_at = datetime('now') \
                 WHERE tenant = ?1 AND entity_type = ?2 AND spec_version = ?3 \
                   AND constraint_version = ?4 AND resolved_at IS NULL",
                    params![
                        resolution.tenant,
                        resolution.entity_type,
                        resolution.quarantined_version,
                        resolution.quarantined_constraint_version.unwrap_or(0)
                    ],
                )
                .await
                .map_err(storage_error)?;
            if changed != 1 && !exact_quarantine_exists(&tx, resolution).await? {
                tx.rollback().await.map_err(storage_error)?;
                return Ok(false);
            }
        }
        let mut active_rows = tx
            .query(
                "SELECT 1 FROM registry_restore_quarantines \
                 WHERE tenant = ?1 AND resolved_at IS NULL LIMIT 1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let has_active = active_rows.next().await.map_err(storage_error)?.is_some();
        drop(active_rows);
        if has_active {
            tx.rollback().await.map_err(storage_error)?;
            return Ok(false);
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    /// Acknowledge an active quarantine without hiding or resolving it.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.acknowledge_registry_restore_quarantine"))]
    pub async fn acknowledge_registry_restore_quarantine(
        &self,
        tenant: &str,
        entity_type: &str,
        spec_version: i64,
        constraint_version: Option<i64>,
    ) -> Result<Option<(i64, Option<i64>)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let mut rows = tx
            .query(
                "SELECT spec_version, constraint_version \
                 FROM registry_restore_quarantines \
                 WHERE tenant = ?1 AND entity_type = ?2 AND resolved_at IS NULL",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        let current = match rows.next().await.map_err(storage_error)? {
            Some(row) => Some((
                row.get::<i64>(0).map_err(storage_error)?,
                row.get::<i64>(1).map_err(storage_error)?,
            )),
            None => None,
        };
        drop(rows);
        if current == Some((spec_version, constraint_version.unwrap_or(0))) {
            tx.execute(
                "UPDATE registry_restore_quarantines \
                 SET acknowledged_at = COALESCE(acknowledged_at, datetime('now')) \
                 WHERE tenant = ?1 AND entity_type = ?2 AND spec_version = ?3 \
                   AND constraint_version = ?4 AND resolved_at IS NULL",
                params![
                    tenant,
                    entity_type,
                    spec_version,
                    constraint_version.unwrap_or(0)
                ],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(current.map(|(spec_version, constraint_version)| {
            (
                spec_version,
                (constraint_version != 0).then_some(constraint_version),
            )
        }))
    }

    /// List active quarantines in deterministic order.
    #[instrument(skip_all, fields(otel.name = "turso.load_registry_restore_quarantines"))]
    pub async fn load_registry_restore_quarantines(
        &self,
    ) -> Result<Vec<TursoRegistryQuarantineRow>, PersistenceError> {
        self.query_registry_restore_quarantines(None).await
    }

    /// List a bounded tenant-scoped active quarantine page.
    pub async fn load_registry_restore_quarantines_for_tenant(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<TursoRegistryQuarantineRow>, PersistenceError> {
        self.query_registry_restore_quarantines(Some((tenant, limit)))
            .await
    }

    async fn query_registry_restore_quarantines(
        &self,
        scope: Option<(&str, usize)>,
    ) -> Result<Vec<TursoRegistryQuarantineRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = match scope {
            Some((tenant, limit)) => {
                let limit = i64::try_from(limit).unwrap_or(i64::MAX); // ci-ok: bounded fallback
                conn.query(
                    "SELECT tenant, entity_type, spec_version, constraint_version, reason, source_kind, source_line, \
                            source_column, detail, acknowledged_at, created_at, last_observed_at \
                     FROM registry_restore_quarantines \
                     WHERE tenant = ?1 AND resolved_at IS NULL \
                     ORDER BY entity_type, spec_version LIMIT ?2",
                    params![tenant, limit],
                )
                .await
            }
            None => {
                conn.query(
                    "SELECT tenant, entity_type, spec_version, constraint_version, reason, source_kind, source_line, \
                        source_column, detail, acknowledged_at, created_at, last_observed_at \
                 FROM registry_restore_quarantines WHERE resolved_at IS NULL \
                 ORDER BY tenant, entity_type, spec_version",
                    (),
                )
                .await
            }
        }
        .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoRegistryQuarantineRow {
                tenant: row.get(0).map_err(storage_error)?,
                entity_type: row.get(1).map_err(storage_error)?,
                spec_version: row.get(2).map_err(storage_error)?,
                constraint_version: match row.get::<i64>(3).map_err(storage_error)? {
                    0 => None,
                    version => Some(version),
                },
                reason: row.get(4).map_err(storage_error)?,
                source_kind: row.get(5).map_err(storage_error)?,
                source_line: row.get(6).map_err(storage_error)?,
                source_column: row.get(7).map_err(storage_error)?,
                detail: row.get(8).map_err(storage_error)?,
                acknowledged_at: row.get(9).map_err(storage_error)?,
                created_at: row.get(10).map_err(storage_error)?,
                last_observed_at: row.get(11).map_err(storage_error)?,
            });
        }
        Ok(out)
    }
}
