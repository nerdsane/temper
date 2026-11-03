//! Per-tenant encrypted secrets: AES-256-GCM ciphertext and nonce, and the
//! opaque version id of the write that produced each row.

use temper_runtime::persistence::{
    PersistenceError, SecretPrecondition, SecretWrite, storage_error,
};

use crate::PostgresEventStore;

/// A single encrypted secret row: `(key_name, ciphertext, nonce)`.
pub type PostgresSecretRow = (String, Vec<u8>, Vec<u8>);
/// A secret's name and the version id of its latest write: `(key_name, version_id)`.
pub type PostgresSecretVersionRow = (String, Option<String>);

impl PostgresEventStore {
    /// Write an encrypted secret if the key's current row meets
    /// `write.precondition`.
    ///
    /// The check and the write are one statement. Two inserts of one absent
    /// key meet at the primary key, and only one inserts; two updates of one
    /// version queue on the row lock, and the second sees the first's new
    /// version id and matches nothing. Returns whether the row was written.
    pub async fn write_secret(&self, write: &SecretWrite<'_>) -> Result<bool, PersistenceError> {
        let SecretWrite {
            tenant,
            key_name,
            ciphertext,
            nonce,
            version_id,
            precondition,
        } = *write;
        let result = match precondition {
            SecretPrecondition::Any => {
                crate::dbm::postgres_query!(
                    &self.schema,
                    "INSERT INTO {schema}tenant_secrets \
                         (tenant, key_name, ciphertext, nonce, version_id, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, now(), now()) \
                     ON CONFLICT (tenant, key_name) DO UPDATE SET \
                         ciphertext = EXCLUDED.ciphertext, \
                         nonce = EXCLUDED.nonce, \
                         version_id = EXCLUDED.version_id, \
                         updated_at = now()",
                )
                .bind(tenant)
                .bind(key_name)
                .bind(ciphertext)
                .bind(nonce)
                .bind(version_id)
                .execute(self.pool())
                .await
            }
            SecretPrecondition::Absent => {
                crate::dbm::postgres_query!(
                    &self.schema,
                    "INSERT INTO {schema}tenant_secrets \
                         (tenant, key_name, ciphertext, nonce, version_id, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, now(), now()) \
                     ON CONFLICT (tenant, key_name) DO NOTHING",
                )
                .bind(tenant)
                .bind(key_name)
                .bind(ciphertext)
                .bind(nonce)
                .bind(version_id)
                .execute(self.pool())
                .await
            }
            SecretPrecondition::Version(expected) => {
                crate::dbm::postgres_query!(
                    &self.schema,
                    "UPDATE {schema}tenant_secrets SET \
                         ciphertext = $3, nonce = $4, version_id = $5, updated_at = now() \
                     WHERE tenant = $1 AND key_name = $2 AND version_id = $6",
                )
                .bind(tenant)
                .bind(key_name)
                .bind(ciphertext)
                .bind(nonce)
                .bind(version_id)
                .bind(expected)
                .execute(self.pool())
                .await
            }
        }
        .map_err(storage_error)?;
        debug_assert!(
            result.rows_affected() <= 1,
            "one (tenant, key_name) row at most"
        );
        Ok(result.rows_affected() == 1)
    }

    pub async fn delete_secret(
        &self,
        tenant: &str,
        key_name: &str,
    ) -> Result<bool, PersistenceError> {
        let result = crate::dbm::postgres_query!(
            &self.schema,
            "DELETE FROM {schema}tenant_secrets WHERE tenant = $1 AND key_name = $2"
        )
        .bind(tenant)
        .bind(key_name)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn load_secrets_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresSecretRow>, PersistenceError> {
        crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT key_name, ciphertext, nonce FROM {schema}tenant_secrets WHERE tenant = $1"
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)
    }

    /// List a tenant's secret names with the version id of each one's latest
    /// write. Never reads a value.
    pub async fn list_secret_versions(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresSecretVersionRow>, PersistenceError> {
        crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT key_name, version_id FROM {schema}tenant_secrets WHERE tenant = $1 \
             ORDER BY key_name"
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)
    }
}
