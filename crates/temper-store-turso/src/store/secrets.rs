//! Per-tenant encrypted secret storage.
//!
//! CRUD operations on the `tenant_secrets` table, storing AES-256-GCM
//! encrypted ciphertext and nonce as BLOBs, and the opaque version id of the
//! write that produced each row.

use crate::driver::params;
use temper_runtime::persistence::{
    PersistenceError, SecretPrecondition, SecretWrite, storage_error,
};
use tracing::instrument;

use super::TursoEventStore;
use super::write_gate::WritePriority;

/// A single encrypted secret row: `(key_name, ciphertext, nonce)`.
pub type SecretRow = (String, Vec<u8>, Vec<u8>);
/// A secret's name and the version id of its latest write: `(key_name, version_id)`.
///
/// The id is `None` only for a row an older release wrote after this
/// release's startup backfill; such a row matches no `If-Match` until it is
/// written again.
pub type SecretVersionRow = (String, Option<String>);
use crate::metrics::TursoQueryTimer;

/// Backfill legacy rows only when a version is missing. Even an UPDATE that
/// changes no rows appends a transaction marker to a synced store's change log.
pub(super) async fn backfill_versions(
    conn: &super::instrumentation::WriteConnection,
) -> Result<(), PersistenceError> {
    // Close the read cursor before updating on the same connection. The write
    // gate is held throughout; conditional PUTs still use one atomic statement.
    let missing = {
        let mut rows = conn
            .query(
                "SELECT 1 FROM tenant_secrets WHERE version_id IS NULL LIMIT 1",
                (),
            )
            .await
            .map_err(storage_error)?;
        rows.next().await.map_err(storage_error)?.is_some()
    };
    if missing {
        conn.execute(crate::schema::BACKFILL_TENANT_SECRETS_VERSION_ID, ())
            .await
            .map_err(storage_error)?;
    }
    Ok(())
}

impl TursoEventStore {
    /// Write an encrypted secret for a tenant if the key's current row meets
    /// `write.precondition`.
    ///
    /// The check and the write are one statement, so of two writers racing on
    /// one key under the same precondition, at most one writes. Returns
    /// whether the row was written.
    #[instrument(skip_all, fields(tenant, key_name, otel.name = "turso.write_secret"))]
    pub async fn write_secret(&self, write: &SecretWrite<'_>) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.write_secret");
        let conn = self
            .write_connection("turso.write_secret", WritePriority::High)
            .await?;
        let SecretWrite {
            tenant,
            key_name,
            ciphertext,
            nonce,
            version_id,
            precondition,
        } = *write;
        let rows_affected = match precondition {
            SecretPrecondition::Any => {
                conn.execute(
                    "INSERT INTO tenant_secrets \
                         (tenant, key_name, ciphertext, nonce, version_id, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), datetime('now')) \
                     ON CONFLICT (tenant, key_name) DO UPDATE SET \
                         ciphertext = excluded.ciphertext, \
                         nonce = excluded.nonce, \
                         version_id = excluded.version_id, \
                         updated_at = datetime('now')",
                    params![tenant, key_name, ciphertext, nonce, version_id],
                )
                .await
            }
            SecretPrecondition::Absent => {
                conn.execute(
                    "INSERT INTO tenant_secrets \
                         (tenant, key_name, ciphertext, nonce, version_id, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), datetime('now')) \
                     ON CONFLICT (tenant, key_name) DO NOTHING",
                    params![tenant, key_name, ciphertext, nonce, version_id],
                )
                .await
            }
            SecretPrecondition::Version(expected) => {
                conn.execute(
                    "UPDATE tenant_secrets SET \
                         ciphertext = ?3, nonce = ?4, version_id = ?5, updated_at = datetime('now') \
                     WHERE tenant = ?1 AND key_name = ?2 AND version_id = ?6",
                    params![tenant, key_name, ciphertext, nonce, version_id, expected],
                )
                .await
            }
        }
        .map_err(storage_error)?;
        debug_assert!(rows_affected <= 1, "one (tenant, key_name) row at most");
        Ok(rows_affected == 1)
    }

    /// Delete a secret by `(tenant, key_name)`.
    ///
    /// Returns `true` if a row was deleted, `false` if no matching row existed.
    #[instrument(skip_all, fields(tenant, key_name, otel.name = "turso.delete_secret"))]
    pub async fn delete_secret(
        &self,
        tenant: &str,
        key_name: &str,
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_secret");
        let conn = self
            .write_connection("turso.delete_secret", WritePriority::High)
            .await?;
        let rows_affected = conn
            .execute(
                "DELETE FROM tenant_secrets WHERE tenant = ?1 AND key_name = ?2",
                params![tenant, key_name],
            )
            .await
            .map_err(storage_error)?;
        Ok(rows_affected > 0)
    }

    /// Load all secrets for a tenant.
    ///
    /// Returns `(key_name, ciphertext, nonce)` triples.  Callers are responsible
    /// for decrypting via [`SecretsVault`].
    #[instrument(skip_all, fields(tenant, otel.name = "turso.load_secrets_for_tenant"))]
    pub async fn load_secrets_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<SecretRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_secrets_for_tenant");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT key_name, ciphertext, nonce FROM tenant_secrets WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let key_name: String = row.get(0).map_err(storage_error)?;
            let ciphertext: Vec<u8> = row.get(1).map_err(storage_error)?;
            let nonce: Vec<u8> = row.get(2).map_err(storage_error)?;
            out.push((key_name, ciphertext, nonce));
        }
        Ok(out)
    }

    /// List a tenant's secret names with the version id of each one's latest
    /// write. Never reads a value.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_secret_versions"))]
    pub async fn list_secret_versions(
        &self,
        tenant: &str,
    ) -> Result<Vec<SecretVersionRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.list_secret_versions");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT key_name, version_id FROM tenant_secrets WHERE tenant = ?1 \
                 ORDER BY key_name",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let key_name: String = row.get(0).map_err(storage_error)?;
            let version_id: Option<String> = row.get(1).map_err(storage_error)?;
            out.push((key_name, version_id));
        }
        Ok(out)
    }
}
