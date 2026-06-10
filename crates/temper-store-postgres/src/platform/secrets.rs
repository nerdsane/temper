//! Tenant secret storage (encrypted key/value rows).

use temper_runtime::persistence::PersistenceError;

use super::storage_error;
use crate::PostgresEventStore;

pub type PostgresSecretRow = (String, Vec<u8>, Vec<u8>);

impl PostgresEventStore {
    pub async fn upsert_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, now(), now()) \
             ON CONFLICT (tenant, key_name) DO UPDATE SET ciphertext = EXCLUDED.ciphertext, nonce = EXCLUDED.nonce, updated_at = now()",
        )
        .bind(tenant)
        .bind(key_name)
        .bind(ciphertext)
        .bind(nonce)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn delete_secret(
        &self,
        tenant: &str,
        key_name: &str,
    ) -> Result<bool, PersistenceError> {
        let result = crate::dbm::postgres_query!(
            "DELETE FROM tenant_secrets WHERE tenant = $1 AND key_name = $2"
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
            "SELECT key_name, ciphertext, nonce FROM tenant_secrets WHERE tenant = $1"
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)
    }
}
