//! Durable registry-restore quarantine persistence.

use std::collections::BTreeMap;

use sqlx::{Postgres, Row, Transaction};
use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::PostgresEventStore;

/// Active quarantine row returned to the server repair API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresRegistryQuarantineRow {
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
    /// RFC 3339 acknowledgment timestamp.
    pub acknowledged_at: Option<String>,
    /// RFC 3339 first-observed timestamp.
    pub created_at: String,
    /// RFC 3339 most-recent-observation timestamp.
    pub last_observed_at: String,
}

/// Versioned quarantine payload produced by one restore attempt.
#[derive(Debug, Clone, Copy)]
pub struct PostgresRegistryQuarantineUpsert<'a> {
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
pub struct PostgresRegistryQuarantineResolution<'a> {
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
pub struct PostgresRegistrySourceSnapshot {
    /// Exact committed spec versions in scope.
    pub spec_versions: BTreeMap<(String, String), i64>,
    /// Exact constraint presence/version for every tenant in scope.
    pub constraint_versions: BTreeMap<String, Option<i64>>,
}

async fn lock_registry_sources(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), PersistenceError> {
    // Restore and repair are control-plane operations. A short shared table
    // lock makes the complete-set comparison and quarantine mutation one CAS,
    // including phantom inserts for new tenants/entities and absent constraint
    // rows. Normal source writers take ROW EXCLUSIVE and therefore wait until
    // this transaction commits.
    sqlx::query("LOCK TABLE specs, tenant_constraints IN SHARE MODE")
        .execute(&mut **transaction)
        .await
        .map_err(storage_error)?;
    // Serialize restore/repair/acknowledgment state changes. Source SHARE locks
    // may coexist, but two quarantine snapshots must not interleave after one
    // has counted zero active rows and before it commits.
    sqlx::query("LOCK TABLE registry_restore_quarantines IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut **transaction)
        .await
        .map_err(storage_error)?;
    Ok(())
}

async fn source_snapshot_matches(
    transaction: &mut Transaction<'_, Postgres>,
    tenant_scope: Option<&str>,
    expected: &PostgresRegistrySourceSnapshot,
) -> Result<bool, PersistenceError> {
    let actual_specs: Vec<(String, String, i64)> = match tenant_scope {
        Some(tenant) => sqlx::query_as(
            "SELECT tenant, entity_type, version::BIGINT FROM specs \
                 WHERE committed = true AND tenant = $1 ORDER BY tenant, entity_type",
        )
        .bind(tenant)
        .fetch_all(&mut **transaction)
        .await
        .map_err(storage_error)?,
        None => sqlx::query_as(
            "SELECT tenant, entity_type, version::BIGINT FROM specs \
                 WHERE committed = true ORDER BY tenant, entity_type",
        )
        .fetch_all(&mut **transaction)
        .await
        .map_err(storage_error)?,
    };
    let actual_spec_versions = actual_specs
        .into_iter()
        .map(|(tenant, entity_type, version)| ((tenant, entity_type), version))
        .collect::<BTreeMap<_, _>>();
    if actual_spec_versions != expected.spec_versions {
        return Ok(false);
    }

    let actual_constraints: Vec<(String, i64)> = match tenant_scope {
        Some(tenant) => sqlx::query_as(
            "SELECT tenant, version::BIGINT FROM tenant_constraints WHERE tenant = $1 ORDER BY tenant",
        )
        .bind(tenant)
        .fetch_all(&mut **transaction)
        .await
        .map_err(storage_error)?,
        None => {
            sqlx::query_as("SELECT tenant, version::BIGINT FROM tenant_constraints ORDER BY tenant")
                .fetch_all(&mut **transaction)
                .await
                .map_err(storage_error)?
        }
    };
    let persisted = actual_constraints.into_iter().collect::<BTreeMap<_, _>>();
    let actual_constraint_versions = actual_spec_versions
        .keys()
        .map(|(tenant, _)| tenant)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|tenant| (tenant.clone(), persisted.get(tenant).copied()))
        .collect::<BTreeMap<_, _>>();
    Ok(actual_constraint_versions == expected.constraint_versions)
}

fn single_resolution_tenant<'a>(
    resolutions: &'a [PostgresRegistryQuarantineResolution<'a>],
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
    transaction: &mut Transaction<'_, Postgres>,
    resolution: &PostgresRegistryQuarantineResolution<'_>,
) -> Result<bool, PersistenceError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM registry_restore_quarantines \
         WHERE tenant = $1 AND entity_type = $2 AND spec_version = $3 \
           AND constraint_version = $4)",
    )
    .bind(resolution.tenant)
    .bind(resolution.entity_type)
    .bind(resolution.quarantined_version)
    .bind(resolution.quarantined_constraint_version.unwrap_or(0))
    .fetch_one(&mut **transaction)
    .await
    .map_err(storage_error)
}

impl PostgresEventStore {
    /// Replace the active restore-quarantine snapshot while retaining resolved history.
    pub async fn replace_registry_restore_quarantines(
        &self,
        source: &PostgresRegistrySourceSnapshot,
        active: &[PostgresRegistryQuarantineUpsert<'_>],
    ) -> Result<bool, PersistenceError> {
        self.replace_registry_restore_quarantines_scoped(None, source, active)
            .await
    }

    /// Replace one tenant's active snapshot without changing other tenants.
    pub async fn replace_registry_restore_quarantines_for_tenant(
        &self,
        tenant: &str,
        source: &PostgresRegistrySourceSnapshot,
        active: &[PostgresRegistryQuarantineUpsert<'_>],
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
        source: &PostgresRegistrySourceSnapshot,
        active: &[PostgresRegistryQuarantineUpsert<'_>],
    ) -> Result<bool, PersistenceError> {
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        lock_registry_sources(&mut tx).await?;
        if !source_snapshot_matches(&mut tx, tenant, source).await? {
            tx.rollback().await.map_err(storage_error)?;
            return Ok(false);
        }
        match tenant {
            Some(tenant) => {
                sqlx::query(
                    "UPDATE registry_restore_quarantines SET resolved_at = now() \
                     WHERE tenant = $1 AND resolved_at IS NULL",
                )
                .bind(tenant)
                .execute(&mut *tx)
                .await
                .map_err(storage_error)?;
            }
            None => {
                sqlx::query(
                    "UPDATE registry_restore_quarantines SET resolved_at = now() \
                     WHERE resolved_at IS NULL",
                )
                .execute(&mut *tx)
                .await
                .map_err(storage_error)?;
            }
        }

        for row in active {
            let changed = sqlx::query(
                "INSERT INTO registry_restore_quarantines \
                 (tenant, entity_type, spec_version, constraint_version, reason, source_kind, source_line, \
                  source_column, detail, created_at, last_observed_at, resolved_at) \
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now(), NULL \
                 WHERE EXISTS (SELECT 1 FROM specs \
                               WHERE specs.tenant = $1 AND specs.entity_type = $2 \
                                 AND specs.version = $3 AND specs.committed = true) \
                   AND (($4 = 0 AND NOT EXISTS (SELECT 1 FROM tenant_constraints \
                                                WHERE tenant_constraints.tenant = $1)) \
                        OR ($4 > 0 AND EXISTS (SELECT 1 FROM tenant_constraints \
                                              WHERE tenant_constraints.tenant = $1 \
                                                AND tenant_constraints.version = $4))) \
                 ON CONFLICT (tenant, entity_type, spec_version, constraint_version) DO UPDATE SET \
                    reason = EXCLUDED.reason, source_kind = EXCLUDED.source_kind, \
                    source_line = EXCLUDED.source_line, source_column = EXCLUDED.source_column, \
                    detail = EXCLUDED.detail, last_observed_at = now(), resolved_at = NULL",
            )
            .bind(row.tenant)
            .bind(row.entity_type)
            .bind(row.spec_version)
            .bind(row.constraint_version.unwrap_or(0))
            .bind(row.reason)
            .bind(row.source_kind)
            .bind(row.source_line)
            .bind(row.source_column)
            .bind(row.detail)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            if changed.rows_affected() != 1 {
                tx.rollback().await.map_err(storage_error)?;
                return Ok(false);
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    /// Atomically resolve an exact set of active records after repair validation.
    pub async fn resolve_registry_restore_quarantines(
        &self,
        source: &PostgresRegistrySourceSnapshot,
        resolutions: &[PostgresRegistryQuarantineResolution<'_>],
    ) -> Result<bool, PersistenceError> {
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        lock_registry_sources(&mut tx).await?;
        let tenant = single_resolution_tenant(resolutions)?;
        if !source_snapshot_matches(&mut tx, Some(tenant), source).await? {
            tx.rollback().await.map_err(storage_error)?;
            return Ok(false);
        }
        for resolution in resolutions {
            let result = sqlx::query(
                "UPDATE registry_restore_quarantines SET resolved_at = now() \
                 WHERE tenant = $1 AND entity_type = $2 AND spec_version = $3 \
                   AND constraint_version = $4 AND resolved_at IS NULL",
            )
            .bind(resolution.tenant)
            .bind(resolution.entity_type)
            .bind(resolution.quarantined_version)
            .bind(resolution.quarantined_constraint_version.unwrap_or(0))
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            if result.rows_affected() != 1 && !exact_quarantine_exists(&mut tx, resolution).await? {
                tx.rollback().await.map_err(storage_error)?;
                return Ok(false);
            }
        }
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM registry_restore_quarantines \
             WHERE tenant = $1 AND resolved_at IS NULL",
        )
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        if active_count != 0 {
            tx.rollback().await.map_err(storage_error)?;
            return Ok(false);
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    /// Acknowledge an active quarantine without hiding or resolving it.
    pub async fn acknowledge_registry_restore_quarantine(
        &self,
        tenant: &str,
        entity_type: &str,
        spec_version: i64,
        constraint_version: Option<i64>,
    ) -> Result<Option<(i64, Option<i64>)>, PersistenceError> {
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        sqlx::query("LOCK TABLE registry_restore_quarantines IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let current = sqlx::query_as::<_, (i64, i64)>(
            "SELECT spec_version, constraint_version \
             FROM registry_restore_quarantines \
             WHERE tenant = $1 AND entity_type = $2 AND resolved_at IS NULL \
             FOR UPDATE",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        if current == Some((spec_version, constraint_version.unwrap_or(0))) {
            sqlx::query(
                "UPDATE registry_restore_quarantines \
                 SET acknowledged_at = COALESCE(acknowledged_at, now()) \
                 WHERE tenant = $1 AND entity_type = $2 AND spec_version = $3 \
                   AND constraint_version = $4 AND resolved_at IS NULL",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(spec_version)
            .bind(constraint_version.unwrap_or(0))
            .execute(&mut *tx)
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
    pub async fn load_registry_restore_quarantines(
        &self,
    ) -> Result<Vec<PostgresRegistryQuarantineRow>, PersistenceError> {
        let rows = sqlx::query(
            "SELECT tenant, entity_type, spec_version, reason, source_kind, source_line, \
                    constraint_version, source_column, detail, acknowledged_at, created_at, last_observed_at \
             FROM registry_restore_quarantines WHERE resolved_at IS NULL \
             ORDER BY tenant, entity_type, spec_version",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;

        Ok(rows.into_iter().map(row_to_quarantine).collect())
    }

    /// List a bounded tenant-scoped active quarantine page.
    pub async fn load_registry_restore_quarantines_for_tenant(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<PostgresRegistryQuarantineRow>, PersistenceError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX); // ci-ok: bounded fallback
        let rows = sqlx::query(
            "SELECT tenant, entity_type, spec_version, reason, source_kind, source_line, \
                    constraint_version, source_column, detail, acknowledged_at, created_at, last_observed_at \
             FROM registry_restore_quarantines \
             WHERE tenant = $1 AND resolved_at IS NULL \
             ORDER BY entity_type, spec_version LIMIT $2",
        )
        .bind(tenant)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_quarantine).collect())
    }
}

fn row_to_quarantine(row: sqlx::postgres::PgRow) -> PostgresRegistryQuarantineRow {
    let acknowledged_at: Option<chrono::DateTime<chrono::Utc>> = row.get("acknowledged_at");
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let last_observed_at: chrono::DateTime<chrono::Utc> = row.get("last_observed_at");
    PostgresRegistryQuarantineRow {
        tenant: row.get("tenant"),
        entity_type: row.get("entity_type"),
        spec_version: row.get("spec_version"),
        constraint_version: match row.get::<i64, _>("constraint_version") {
            0 => None,
            version => Some(version),
        },
        reason: row.get("reason"),
        source_kind: row.get("source_kind"),
        source_line: row.get("source_line"),
        source_column: row.get("source_column"),
        detail: row.get("detail"),
        acknowledged_at: acknowledged_at.map(|value| value.to_rfc3339()),
        created_at: created_at.to_rfc3339(),
        last_observed_at: last_observed_at.to_rfc3339(),
    }
}
