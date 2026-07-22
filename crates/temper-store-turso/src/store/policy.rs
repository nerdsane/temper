//! Cedar policy persistence — granular per-policy storage with hash-based change detection.
//!
//! Provides CRUD operations on the `policies` table, which tracks individual Cedar
//! policy entries per tenant.  Unlike the legacy `tenant_policies` table (one blob
//! per tenant), this table supports multiple entries per tenant, each with its own
//! `policy_id`, content hash, enabled flag, and audit fields.

use libsql::{TransactionBehavior, params};
use sha2::{Digest, Sha256};
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, write_gate::WritePriority};
use crate::metrics::TursoQueryTimer;

mod denial_patterns;

/// A row from the `policies` table.
#[derive(Debug, Clone)]
pub struct PolicyRow {
    /// Tenant that owns this policy.
    pub tenant: String,
    /// Logical policy identifier within the tenant (e.g. "primary", "decision:{id}").
    pub policy_id: String,
    /// Raw Cedar policy text.
    pub cedar_text: String,
    /// SHA-256 hex digest of `cedar_text` — used for change detection.
    pub policy_hash: String,
    /// ISO-8601 timestamp when this row was last written.
    pub created_at: String,
    /// Identity that wrote this policy (agent ID, "api", "system", etc.).
    pub created_by: String,
    /// Whether this policy is active.  Disabled policies are stored but not loaded
    /// into the Cedar engine at boot or reload.
    pub enabled: bool,
}

/// One entry in an atomically published tenant policy generation.
#[derive(Debug, Clone)]
pub struct PolicyGenerationWrite<'a> {
    /// Stable row identifier within the tenant.
    pub policy_id: &'a str,
    /// Raw Cedar source for the row.
    pub cedar_text: &'a str,
    /// Whether the row participates in the live generation.
    pub enabled: bool,
    /// Identity responsible for the row mutation.
    pub created_by: &'a str,
}

impl TursoEventStore {
    /// Load one tenant's compatibility policy projection.
    pub async fn load_tenant_policy(
        &self,
        tenant: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT policy_text FROM tenant_policies WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        rows.next()
            .await
            .map_err(storage_error)?
            .map(|row| row.get::<String>(0).map_err(storage_error))
            .transpose()
    }

    /// Atomically replace both representations of one tenant policy generation.
    pub async fn replace_policy_generation(
        &self,
        tenant: &str,
        entries: &[PolicyGenerationWrite<'_>],
        compatibility_text: &str,
    ) -> Result<(), PersistenceError> {
        let mut policy_ids = std::collections::BTreeSet::new();
        for entry in entries {
            if entry.policy_id.is_empty() || !policy_ids.insert(entry.policy_id) {
                return Err(PersistenceError::Storage(format!(
                    "policy generation for tenant '{tenant}' contains an empty or duplicate policy id"
                )));
            }
        }

        let conn = self.configured_connection().await?;
        let _write_permit = self
            .acquire_write_permit("turso.replace_policy_generation", WritePriority::High)
            .await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        tx.execute("DELETE FROM policies WHERE tenant = ?1", params![tenant])
            .await
            .map_err(storage_error)?;
        for entry in entries {
            let policy_hash = compute_policy_hash(entry.cedar_text);
            let enabled = i32::from(entry.enabled);
            tx.execute(
                "INSERT INTO policies \
                 (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
                 VALUES (?1, ?2, ?3, ?4, datetime('now'), ?5, ?6)",
                params![
                    tenant,
                    entry.policy_id,
                    entry.cedar_text,
                    policy_hash,
                    entry.created_by,
                    enabled
                ],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.execute(
            "INSERT INTO tenant_policies (tenant, policy_text, updated_at) \
             VALUES (?1, ?2, datetime('now')) \
             ON CONFLICT(tenant) DO UPDATE SET \
                 policy_text = excluded.policy_text, updated_at = datetime('now')",
            params![tenant, compatibility_text],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)
    }

    /// Persist a Cedar policy entry for a tenant.
    ///
    /// Computes a SHA-256 hash of `cedar_text` and compares it against any
    /// existing row for `(tenant, policy_id)`.  If the hash matches, no write
    /// is issued and the method returns `Ok(false)`.  On a content change (or
    /// first insert) the row is upserted and `Ok(true)` is returned.
    ///
    /// Callers can use the boolean return value to decide whether to log a
    /// trajectory entry for the change.
    #[instrument(skip_all, fields(tenant, policy_id, otel.name = "turso.save_policy"))]
    pub async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.save_policy");
        let policy_hash = compute_policy_hash(cedar_text);
        let conn = self.configured_connection().await?;

        // A matching but disabled row is not a no-op: publication promises to
        // make the candidate generation durable and live across restart.
        let existing: Option<(String, bool)> = {
            let mut rows = conn
                .query(
                    "SELECT policy_hash, enabled FROM policies \
                     WHERE tenant = ?1 AND policy_id = ?2",
                    params![tenant, policy_id],
                )
                .await
                .map_err(storage_error)?;
            match rows.next().await.map_err(storage_error)? {
                Some(row) => Some((
                    row.get::<String>(0).map_err(storage_error)?,
                    row.get::<i64>(1).map_err(storage_error)? != 0,
                )),
                None => None,
            }
        };

        if existing
            .as_ref()
            .is_some_and(|(hash, enabled)| hash == &policy_hash && *enabled)
        {
            tracing::debug!(
                tenant,
                policy_id,
                "Cedar policy unchanged (hash match), skipping write"
            );
            return Ok(false);
        }

        conn.execute(
            "INSERT INTO policies \
             (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
             VALUES (?1, ?2, ?3, ?4, datetime('now'), ?5, 1) \
             ON CONFLICT(tenant, policy_id) DO UPDATE SET \
                 cedar_text   = excluded.cedar_text, \
                 policy_hash  = excluded.policy_hash, \
                 created_by   = excluded.created_by, \
                 created_at   = datetime('now'), \
                 enabled      = 1",
            params![
                tenant,
                policy_id,
                cedar_text,
                policy_hash.clone(),
                created_by
            ],
        )
        .await
        .map_err(storage_error)?;

        tracing::info!(
            tenant,
            policy_id,
            hash = %policy_hash,
            created_by,
            "Cedar policy persisted to Turso"
        );
        Ok(true)
    }

    /// Load all Cedar policy rows for a tenant, ordered by creation time (oldest first).
    ///
    /// Returns all policies (enabled and disabled).  Callers that need to build the
    /// effective Cedar policy set should filter on `enabled == true`.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.load_policies_for_tenant"))]
    pub async fn load_policies_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_policies_for_tenant");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled \
                 FROM policies \
                 WHERE tenant = ?1 \
                 ORDER BY created_at ASC",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(PolicyRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                policy_id: row.get::<String>(1).map_err(storage_error)?,
                cedar_text: row.get::<String>(2).map_err(storage_error)?,
                policy_hash: row.get::<String>(3).map_err(storage_error)?,
                created_at: row.get::<String>(4).map_err(storage_error)?,
                created_by: row.get::<String>(5).map_err(storage_error)?,
                enabled: row.get::<i32>(6).map_err(storage_error)? != 0,
            });
        }
        Ok(out)
    }

    /// Load all Cedar policy rows across all tenants, ordered by tenant then creation time.
    ///
    /// Used by the cross-tenant Observe UI policies view.
    #[instrument(skip_all, fields(otel.name = "turso.load_all_policies"))]
    pub async fn load_all_policies(&self) -> Result<Vec<PolicyRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_all_policies");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled \
                 FROM policies \
                 ORDER BY tenant ASC, created_at ASC",
                params![],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(PolicyRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                policy_id: row.get::<String>(1).map_err(storage_error)?,
                cedar_text: row.get::<String>(2).map_err(storage_error)?,
                policy_hash: row.get::<String>(3).map_err(storage_error)?,
                created_at: row.get::<String>(4).map_err(storage_error)?,
                created_by: row.get::<String>(5).map_err(storage_error)?,
                enabled: row.get::<i32>(6).map_err(storage_error)? != 0,
            });
        }
        Ok(out)
    }

    /// Toggle the `enabled` flag for a single Cedar policy entry.
    ///
    /// Returns `Ok(true)` if the row existed and was updated, `Ok(false)` if no
    /// matching row was found.
    #[instrument(skip_all, fields(tenant, policy_id, enabled, otel.name = "turso.toggle_policy_enabled"))]
    pub async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.toggle_policy_enabled");
        let conn = self.configured_connection().await?;
        let enabled_int: i32 = if enabled { 1 } else { 0 };
        let affected = conn
            .execute(
                "UPDATE policies SET enabled = ?3 \
                 WHERE tenant = ?1 AND policy_id = ?2",
                params![tenant, policy_id, enabled_int],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected > 0)
    }

    /// Update the Cedar text for an existing policy entry.
    ///
    /// Returns `Ok(true)` if the row existed and was updated, `Ok(false)` if no
    /// matching row was found.
    #[instrument(skip_all, fields(tenant, policy_id, otel.name = "turso.update_policy_text"))]
    pub async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.update_policy_text");
        let policy_hash = compute_policy_hash(cedar_text);
        let conn = self.configured_connection().await?;
        let affected = conn
            .execute(
                "UPDATE policies SET cedar_text = ?3, policy_hash = ?4, created_by = ?5, \
                 created_at = datetime('now') \
                 WHERE tenant = ?1 AND policy_id = ?2",
                params![tenant, policy_id, cedar_text, policy_hash, created_by],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected > 0)
    }

    /// Atomically replace the text and enabled state of one Cedar policy.
    #[instrument(skip_all, fields(tenant, policy_id, otel.name = "turso.replace_policy"))]
    pub async fn replace_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        enabled: bool,
        created_by: &str,
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.replace_policy");
        let policy_hash = compute_policy_hash(cedar_text);
        let enabled_int: i32 = if enabled { 1 } else { 0 };
        let conn = self.configured_connection().await?;
        let affected = conn
            .execute(
                "UPDATE policies \
                 SET cedar_text = ?3, policy_hash = ?4, enabled = ?5, \
                     created_by = ?6, created_at = datetime('now') \
                 WHERE tenant = ?1 AND policy_id = ?2",
                params![
                    tenant,
                    policy_id,
                    cedar_text,
                    policy_hash,
                    enabled_int,
                    created_by
                ],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected > 0)
    }

    /// Delete a single Cedar policy entry by `(tenant, policy_id)`.
    ///
    /// Silently succeeds if the row does not exist.
    #[instrument(skip_all, fields(tenant, policy_id, otel.name = "turso.delete_policy"))]
    pub async fn delete_policy(
        &self,
        tenant: &str,
        policy_id: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_policy");
        let conn = self.configured_connection().await?;
        conn.execute(
            "DELETE FROM policies WHERE tenant = ?1 AND policy_id = ?2",
            params![tenant, policy_id],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }
}

/// Compute a SHA-256 hex digest of Cedar policy text.
///
/// Identical inputs always produce the same digest, enabling cheap change
/// detection before issuing an expensive Turso write.
fn compute_policy_hash(cedar_text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cedar_text.as_bytes());
    format!("{:x}", hasher.finalize())
}
