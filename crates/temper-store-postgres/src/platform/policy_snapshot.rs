//! Versioned atomic publication of complete tenant policy snapshots.

use sha2::{Digest, Sha256};
use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{PostgresPolicyRow, compute_policy_hash, row_to_policy};
use crate::PostgresEventStore;

/// One complete durable policy snapshot and its publication version.
#[derive(Debug, Clone)]
pub struct PostgresPolicySnapshot {
    pub version: u64,
    pub rows: Vec<PostgresPolicyRow>,
}

/// One row supplied to an atomic snapshot replacement.
#[derive(Debug, Clone)]
pub struct PostgresPolicySnapshotEntry {
    pub policy_id: String,
    pub cedar_text: String,
    pub created_at: String,
    pub created_by: String,
    pub enabled: bool,
}

fn snapshot_hash(rows: &[PostgresPolicySnapshotEntry]) -> String {
    let mut hasher = Sha256::new();
    for row in rows {
        for component in [
            row.policy_id.as_bytes(),
            row.cedar_text.as_bytes(),
            row.created_at.as_bytes(),
            row.created_by.as_bytes(),
            if row.enabled { b"1" } else { b"0" },
        ] {
            hasher.update((component.len() as u64).to_be_bytes());
            hasher.update(component);
        }
    }
    format!("{:x}", hasher.finalize())
}

impl PostgresEventStore {
    /// Load rows and publication head from one repeatable database snapshot.
    pub async fn load_policy_snapshot(
        &self,
        tenant: &str,
    ) -> Result<PostgresPolicySnapshot, PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        let version = sqlx::query_scalar::<_, i64>(
            "SELECT version FROM policy_publications WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?
        .unwrap_or(0);
        let rows = sqlx::query(
            "SELECT tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled \
             FROM policies WHERE tenant = $1 ORDER BY policy_id ASC",
        )
        .bind(tenant)
        .fetch_all(&mut *transaction)
        .await
        .map_err(storage_error)?
        .into_iter()
        .map(row_to_policy)
        .collect();
        transaction.commit().await.map_err(storage_error)?;
        Ok(PostgresPolicySnapshot {
            version: u64::try_from(version).map_err(|_| {
                PersistenceError::Storage("negative policy publication version".to_string())
            })?,
            rows,
        })
    }

    /// Replace the complete tenant set if its publication version still matches.
    pub async fn replace_policy_snapshot(
        &self,
        tenant: &str,
        expected_version: u64,
        mut rows: Vec<PostgresPolicySnapshotEntry>,
    ) -> Result<u64, PersistenceError> {
        rows.sort_by(|left, right| left.policy_id.cmp(&right.policy_id));
        for pair in rows.windows(2) {
            if pair[0].policy_id == pair[1].policy_id {
                return Err(PersistenceError::Serialization(format!(
                    "duplicate policy id {:?} in snapshot",
                    pair[0].policy_id
                )));
            }
        }
        let expected_i64 = i64::try_from(expected_version).map_err(|_| {
            PersistenceError::Serialization("policy publication version exceeds i64".to_string())
        })?;
        let next_version = expected_version.checked_add(1).ok_or_else(|| {
            PersistenceError::Serialization("policy publication version exhausted".to_string())
        })?;
        let next_i64 = i64::try_from(next_version).map_err(|_| {
            PersistenceError::Serialization("policy publication version exceeds i64".to_string())
        })?;
        let hash = snapshot_hash(&rows);
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        sqlx::query(
            "INSERT INTO policy_publications (tenant, version, snapshot_hash) \
             VALUES ($1, 0, '') ON CONFLICT (tenant) DO NOTHING",
        )
        .bind(tenant)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        let actual = sqlx::query_scalar::<_, i64>(
            "SELECT version FROM policy_publications WHERE tenant = $1 FOR UPDATE",
        )
        .bind(tenant)
        .fetch_one(&mut *transaction)
        .await
        .map_err(storage_error)?;
        if actual != expected_i64 {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_version,
                actual: u64::try_from(actual).unwrap_or(0),
            });
        }

        sqlx::query("DELETE FROM policies WHERE tenant = $1")
            .bind(tenant)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        for row in &rows {
            let created_at = chrono::DateTime::parse_from_rfc3339(&row.created_at)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?
                .with_timezone(&chrono::Utc);
            sqlx::query(
                "INSERT INTO policies \
                 (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(tenant)
            .bind(&row.policy_id)
            .bind(&row.cedar_text)
            .bind(compute_policy_hash(&row.cedar_text))
            .bind(created_at)
            .bind(&row.created_by)
            .bind(row.enabled)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        }
        let updated = sqlx::query(
            "UPDATE policy_publications SET version = $2, snapshot_hash = $3, updated_at = now() \
             WHERE tenant = $1 AND version = $4",
        )
        .bind(tenant)
        .bind(next_i64)
        .bind(hash)
        .bind(expected_i64)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        if updated.rows_affected() != 1 {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_version,
                actual: expected_version,
            });
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(next_version)
    }
}
