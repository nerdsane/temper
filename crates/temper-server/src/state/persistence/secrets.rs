//! Encrypted tenant secrets in the tenant's metadata store.

use std::collections::BTreeMap;

use temper_runtime::persistence::{SecretPrecondition, SecretWrite};

use super::super::ServerState;
use super::TenantMetadataBackend;

/// What a secret write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretWriteOutcome {
    /// The row was written and now carries this version id.
    Written { version_id: String },
    /// The key's current row did not meet the precondition; nothing changed.
    PreconditionFailed,
}

impl ServerState {
    /// Store an encrypted secret if the key's current row meets `precondition`.
    ///
    /// The store checks and writes in one statement. Each write gets a new
    /// version id from `sim_uuid` (a UUIDv7 in production, seeded under
    /// simulation); see [`SecretWrite::version_id`] for why it is never
    /// derived from the value.
    pub async fn write_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
        precondition: SecretPrecondition<'_>,
    ) -> Result<SecretWriteOutcome, String> {
        let version_id = temper_runtime::scheduler::sim_uuid().simple().to_string();
        let write = SecretWrite {
            tenant,
            key_name,
            ciphertext,
            nonce,
            version_id: &version_id,
            precondition,
        };
        let written = match self.tenant_metadata_backend(tenant).await {
            // No store: the secret lives only in this server's memory, as
            // before. A condition has nothing durable to be checked against.
            None if precondition == SecretPrecondition::Any => true,
            None => {
                return Err(
                    "conditional secret writes are not supported without a metadata store".into(),
                );
            }
            Some(TenantMetadataBackend::Postgres(store)) => store
                .write_secret(&write)
                .await
                .map_err(|e| format!("failed to write secret {tenant}/{key_name}: {e}"))?,
            Some(TenantMetadataBackend::Turso(turso)) => turso
                .write_secret(&write)
                .await
                .map_err(|e| format!("failed to write secret {tenant}/{key_name} in turso: {e}"))?,
            Some(TenantMetadataBackend::Redis) => {
                return Err(Self::redis_ephemeral_error("Secret persistence"));
            }
        };
        Ok(if written {
            SecretWriteOutcome::Written { version_id }
        } else {
            SecretWriteOutcome::PreconditionFailed
        })
    }

    /// Delete a secret from the persistence backend.
    pub async fn delete_secret(&self, tenant: &str, key_name: &str) -> Result<bool, String> {
        match self.tenant_metadata_backend(tenant).await {
            None => Ok(false),
            Some(TenantMetadataBackend::Postgres(store)) => store
                .delete_secret(tenant, key_name)
                .await
                .map_err(|e| format!("failed to delete secret {tenant}/{key_name}: {e}")),
            Some(TenantMetadataBackend::Turso(turso)) => turso
                .delete_secret(tenant, key_name)
                .await
                .map_err(|e| format!("failed to delete secret {tenant}/{key_name} in turso: {e}")),
            Some(TenantMetadataBackend::Redis) => {
                Err(Self::redis_ephemeral_error("Secret deletion"))
            }
        }
    }

    /// The version id of each stored secret of a tenant, by name. Never reads
    /// a value. A row with no version id yet (written by an older release) is
    /// left out, as are platform secrets, which have no row.
    pub async fn secret_versions(&self, tenant: &str) -> Result<BTreeMap<String, String>, String> {
        let rows = match self.tenant_metadata_backend(tenant).await {
            // No store, or Redis, which refuses every secret write: no rows.
            None | Some(TenantMetadataBackend::Redis) => Vec::new(),
            Some(TenantMetadataBackend::Postgres(store)) => store
                .list_secret_versions(tenant)
                .await
                .map_err(|e| format!("failed to list secrets for tenant {tenant}: {e}"))?,
            Some(TenantMetadataBackend::Turso(turso)) => {
                turso.list_secret_versions(tenant).await.map_err(|e| {
                    format!("failed to list secrets for tenant {tenant} in turso: {e}")
                })?
            }
        };
        Ok(rows
            .into_iter()
            .filter_map(|(key_name, version_id)| version_id.map(|id| (key_name, id)))
            .collect())
    }

    /// Load all secrets for a tenant from persistence, decrypt, and cache.
    pub async fn load_tenant_secrets(&self, tenant: &str) -> Result<usize, String> {
        let Some(vault) = self.secrets_vault.as_ref() else {
            return Ok(0);
        };
        let rows = match self.tenant_metadata_backend(tenant).await {
            None => return Ok(0),
            Some(TenantMetadataBackend::Postgres(store)) => store
                .load_secrets_for_tenant(tenant)
                .await
                .map_err(|e| format!("failed to load secrets for tenant {tenant}: {e}"))?,
            Some(TenantMetadataBackend::Turso(turso)) => {
                turso.load_secrets_for_tenant(tenant).await.map_err(|e| {
                    format!("failed to load secrets for tenant {tenant} from turso: {e}")
                })?
            }
            Some(TenantMetadataBackend::Redis) => {
                return Err(Self::redis_ephemeral_error("Secret loading"));
            }
        };

        let mut count = 0;
        for (key_name, ciphertext, nonce) in &rows {
            match vault.decrypt(ciphertext, nonce) {
                Ok(plaintext) => {
                    let value = String::from_utf8(plaintext)
                        .map_err(|e| format!("secret {key_name} is not valid UTF-8: {e}"))?;
                    vault.cache_secret(tenant, key_name, value);
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        tenant,
                        key_name,
                        error = %e,
                        "failed to decrypt secret, skipping"
                    );
                }
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::ActorSystem;
    use temper_runtime::persistence::SecretPrecondition;
    use temper_store_turso::TursoEventStore;

    use super::SecretWriteOutcome;
    use crate::registry::SpecRegistry;
    use crate::secrets::vault::SecretsVault;
    use crate::state::ServerState;
    use crate::storage::StorageStack;

    fn make_state() -> ServerState {
        let system = ActorSystem::new("test-secrets-persistence");
        ServerState::from_registry(system, SpecRegistry::new())
            .with_secrets_vault(SecretsVault::new(&[7u8; 32]))
    }

    #[tokio::test]
    async fn turso_secret_round_trip() {
        let db_path =
            std::env::temp_dir().join(format!("temper-secrets-{}.db", uuid::Uuid::new_v4())); // determinism-ok: test-only temp file
        let db_url = format!("file:{}", db_path.display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create local turso db");

        let mut state = make_state();
        state.set_storage_stack(StorageStack::from_turso(store));

        let vault = state.secrets_vault.as_ref().expect("vault configured");
        let (ciphertext, nonce) = vault.encrypt(b"secret-value").expect("encrypt");

        let outcome = state
            .write_secret(
                "tenant-a",
                "API_KEY",
                &ciphertext,
                &nonce,
                SecretPrecondition::Any,
            )
            .await
            .expect("turso secret write should succeed");
        let SecretWriteOutcome::Written { version_id } = outcome else {
            panic!("an unconditional write is always written");
        };
        assert_eq!(
            state.secret_versions("tenant-a").await.unwrap()["API_KEY"],
            version_id
        );

        // Load and decrypt.
        let loaded = state
            .load_tenant_secrets("tenant-a")
            .await
            .expect("turso secret load should succeed");
        assert_eq!(loaded, 1, "should have loaded 1 secret");

        // Verify the decrypted value is cached.
        let cached = vault
            .get_secret("tenant-a", "API_KEY")
            .expect("secret should be cached");
        assert_eq!(cached, "secret-value");

        // Delete secret.
        let deleted = state
            .delete_secret("tenant-a", "API_KEY")
            .await
            .expect("turso secret delete should succeed");
        assert!(deleted, "should have deleted 1 row");

        let _ = std::fs::remove_file(db_path); // determinism-ok: test-only cleanup
    }
}
