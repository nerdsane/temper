//! Versioned atomic publication of complete tenant policy snapshots.

use libsql::{TransactionBehavior, params};
use sha2::{Digest, Sha256};
use temper_runtime::persistence::{PersistenceError, storage_error};

use super::TursoEventStore;
use super::policy::{PolicyRow, compute_policy_hash};

/// One complete durable policy snapshot and its publication version.
#[derive(Debug, Clone)]
pub struct PolicySnapshot {
    pub version: u64,
    pub rows: Vec<PolicyRow>,
}

/// One row supplied to an atomic snapshot replacement.
#[derive(Debug, Clone)]
pub struct PolicySnapshotEntry {
    pub policy_id: String,
    pub cedar_text: String,
    pub created_at: String,
    pub created_by: String,
    pub enabled: bool,
}

fn snapshot_hash(rows: &[PolicySnapshotEntry]) -> String {
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

impl TursoEventStore {
    /// Load rows and publication head from one database read transaction.
    pub async fn load_policy_snapshot(
        &self,
        tenant: &str,
    ) -> Result<PolicySnapshot, PersistenceError> {
        let connection = self.configured_connection().await?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .await
            .map_err(storage_error)?;
        let version = {
            let mut versions = transaction
                .query(
                    "SELECT version FROM policy_publications WHERE tenant = ?1",
                    params![tenant],
                )
                .await
                .map_err(storage_error)?;
            versions
                .next()
                .await
                .map_err(storage_error)?
                .map(|row| row.get::<i64>(0).map_err(storage_error))
                .transpose()?
                .unwrap_or(0)
        };
        let mut query = transaction
            .query(
                "SELECT tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled \
                 FROM policies WHERE tenant = ?1 ORDER BY policy_id ASC",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let mut rows = Vec::new();
        while let Some(row) = query.next().await.map_err(storage_error)? {
            rows.push(PolicyRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                policy_id: row.get::<String>(1).map_err(storage_error)?,
                cedar_text: row.get::<String>(2).map_err(storage_error)?,
                policy_hash: row.get::<String>(3).map_err(storage_error)?,
                created_at: row.get::<String>(4).map_err(storage_error)?,
                created_by: row.get::<String>(5).map_err(storage_error)?,
                enabled: row.get::<i64>(6).map_err(storage_error)? != 0,
            });
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(PolicySnapshot {
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
        mut rows: Vec<PolicySnapshotEntry>,
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
        let connection = self.configured_connection().await?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        transaction
            .execute(
                "INSERT INTO policy_publications (tenant, version, snapshot_hash) \
                 VALUES (?1, 0, '') ON CONFLICT(tenant) DO NOTHING",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let actual = {
            let mut versions = transaction
                .query(
                    "SELECT version FROM policy_publications WHERE tenant = ?1",
                    params![tenant],
                )
                .await
                .map_err(storage_error)?;
            versions
                .next()
                .await
                .map_err(storage_error)?
                .ok_or_else(|| {
                    PersistenceError::Storage("missing policy publication head".to_string())
                })?
                .get::<i64>(0)
                .map_err(storage_error)?
        };
        if actual != expected_i64 {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_version,
                actual: u64::try_from(actual).map_err(|_| {
                    PersistenceError::Storage("negative policy publication version".to_string())
                })?,
            });
        }
        transaction
            .execute("DELETE FROM policies WHERE tenant = ?1", params![tenant])
            .await
            .map_err(storage_error)?;
        for row in &rows {
            transaction
                .execute(
                    "INSERT INTO policies \
                     (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        tenant,
                        row.policy_id.clone(),
                        row.cedar_text.clone(),
                        compute_policy_hash(&row.cedar_text),
                        row.created_at.clone(),
                        row.created_by.clone(),
                        i64::from(row.enabled),
                    ],
                )
                .await
                .map_err(storage_error)?;
        }
        let updated = transaction
            .execute(
                "UPDATE policy_publications \
                 SET version = ?2, snapshot_hash = ?3, updated_at = datetime('now') \
                 WHERE tenant = ?1 AND version = ?4",
                params![tenant, next_i64, hash, expected_i64],
            )
            .await
            .map_err(storage_error)?;
        if updated != 1 {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_version,
                actual: expected_version,
            });
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(next_version)
    }
}
