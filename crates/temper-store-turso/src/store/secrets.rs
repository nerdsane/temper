//! Per-tenant encrypted secret storage.
//!
//! CRUD operations on the `tenant_secrets` table, storing AES-256-GCM
//! encrypted ciphertext and nonce as BLOBs.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;

/// A single encrypted secret row: `(key_name, ciphertext, nonce)`.
pub type SecretRow = (String, Vec<u8>, Vec<u8>);
use crate::metrics::TursoQueryTimer;

impl TursoEventStore {
    /// Upsert an encrypted secret for a tenant.
    ///
    /// If a row with the same `(tenant, key_name)` exists, both the ciphertext
    /// and nonce are replaced and `updated_at` is refreshed.
    #[instrument(skip_all, fields(tenant, key_name, otel.name = "turso.upsert_secret"))]
    pub async fn upsert_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_secret");
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, datetime('now'), datetime('now')) \
             ON CONFLICT (tenant, key_name) DO UPDATE SET \
                 ciphertext = excluded.ciphertext, \
                 nonce = excluded.nonce, \
                 updated_at = datetime('now')",
            params![tenant, key_name, ciphertext, nonce],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
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
        let conn = self.configured_connection().await?;
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
}
