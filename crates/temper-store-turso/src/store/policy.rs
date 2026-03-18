//! Cedar policy persistence — granular per-policy storage with hash-based change detection.
//!
//! Provides CRUD operations on the `policies` table, which tracks individual Cedar
//! policy entries per tenant.  Unlike the legacy `tenant_policies` table (one blob
//! per tenant), this table supports multiple entries per tenant, each with its own
//! `policy_id`, content hash, enabled flag, and audit fields.

use libsql::params;
use sha2::{Digest, Sha256};
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use crate::metrics::TursoQueryTimer;

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

impl TursoEventStore {
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

        // Check existing hash to avoid redundant writes.
        let existing_hash: Option<String> = {
            let mut rows = conn
                .query(
                    "SELECT policy_hash FROM policies \
                     WHERE tenant = ?1 AND policy_id = ?2",
                    params![tenant, policy_id],
                )
                .await
                .map_err(storage_error)?;
            match rows.next().await.map_err(storage_error)? {
                Some(row) => Some(row.get::<String>(0).map_err(storage_error)?),
                None => None,
            }
        };

        if existing_hash.as_deref() == Some(policy_hash.as_str()) {
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
                 created_at   = datetime('now')",
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
