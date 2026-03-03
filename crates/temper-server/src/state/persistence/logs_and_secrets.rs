use temper_runtime::scheduler::sim_now;
use temper_store_turso::TursoTrajectoryInsert;

use super::super::trajectory::TrajectoryEntry;
use super::super::{DesignTimeEvent, ServerState, TRAJECTORY_LOG_CAPACITY};
use super::MetadataBackend;

impl ServerState {
    /// Broadcast and persist a design-time event.
    pub async fn emit_design_time_event(&self, event: DesignTimeEvent) -> Result<(), String> {
        let Some(pool) = self
            .event_store
            .as_ref()
            .and_then(|store| store.postgres_pool())
        else {
            let _ = self.design_time_tx.send(event.clone());
            self.push_design_time_event(event);
            return Ok(());
        };
        let created_at = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| sim_now());
        sqlx::query(
            "INSERT INTO design_time_events \
             (kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&event.kind)
        .bind(&event.entity_type)
        .bind(&event.tenant)
        .bind(&event.summary)
        .bind(event.level.as_deref())
        .bind(event.passed)
        .bind(event.step_number.map(i16::from))
        .bind(event.total_steps.map(i16::from))
        .bind(created_at)
        .execute(pool)
        .await
        .map_err(|e| {
            format!(
                "failed to persist design-time event {} for {}/{}: {e}",
                event.kind, event.tenant, event.entity_type
            )
        })?;
        let _ = self.design_time_tx.send(event.clone());
        self.push_design_time_event(event);
        Ok(())
    }

    /// Persist a trajectory entry (Postgres, Turso, or Redis).
    pub async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(());
        };

        // Try Postgres first.
        if let Some(pool) = store.postgres_pool() {
            let created_at = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| sim_now());
            sqlx::query(
                "INSERT INTO trajectories \
                 (tenant, entity_type, entity_id, action, success, from_status, to_status, error, agent_id, session_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(&entry.tenant)
            .bind(&entry.entity_type)
            .bind(&entry.entity_id)
            .bind(&entry.action)
            .bind(entry.success)
            .bind(entry.from_status.as_deref())
            .bind(entry.to_status.as_deref())
            .bind(entry.error.as_deref())
            .bind(entry.agent_id.as_deref())
            .bind(entry.session_id.as_deref())
            .bind(created_at)
            .execute(pool)
            .await
            .map_err(|e| {
                format!(
                    "failed to persist trajectory entry for {}/{}/{} action {}: {e}",
                    entry.tenant, entry.entity_type, entry.entity_id, entry.action
                )
            })?;
            return Ok(());
        }

        // Fall back to Turso.
        if let Some(turso) = store.turso_store() {
            turso
                .persist_trajectory(TursoTrajectoryInsert {
                    tenant: &entry.tenant,
                    entity_type: &entry.entity_type,
                    entity_id: &entry.entity_id,
                    action: &entry.action,
                    success: entry.success,
                    from_status: entry.from_status.as_deref(),
                    to_status: entry.to_status.as_deref(),
                    error: entry.error.as_deref(),
                    created_at: &entry.timestamp,
                })
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist trajectory entry for {}/{}/{} action {} in turso: {e}",
                        entry.tenant, entry.entity_type, entry.entity_id, entry.action
                    )
                })?;
            return Ok(());
        }

        // Fall back to Redis (capped list).
        if let Some(redis) = store.redis_store() {
            let entry_json = serde_json::to_string(entry)
                .map_err(|e| format!("failed to serialize trajectory entry: {e}"))?;
            redis
                .persist_trajectory(&entry.tenant, &entry_json, TRAJECTORY_LOG_CAPACITY as i64)
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist trajectory entry for {}/{}/{} action {} in redis: {e}",
                        entry.tenant, entry.entity_type, entry.entity_id, entry.action
                    )
                })?;
        }

        Ok(())
    }

    /// Upsert an encrypted secret in the persistence backend.
    pub async fn upsert_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<(), String> {
        let Some(backend) = self.metadata_backend() else {
            return Ok(());
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, now(), now()) \
                     ON CONFLICT (tenant, key_name) DO UPDATE SET \
                         ciphertext = EXCLUDED.ciphertext, \
                         nonce = EXCLUDED.nonce, \
                         updated_at = now()",
                )
                .bind(tenant)
                .bind(key_name)
                .bind(ciphertext)
                .bind(nonce)
                .execute(pool)
                .await
                .map_err(|e| format!("failed to upsert secret {tenant}/{key_name}: {e}"))?;
                Ok(())
            }
            MetadataBackend::Turso(_) => {
                Err("secret persistence is not supported on turso backend yet".to_string())
            }
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("Secret persistence")),
        }
    }

    /// Delete a secret from the persistence backend.
    pub async fn delete_secret(&self, tenant: &str, key_name: &str) -> Result<bool, String> {
        let Some(backend) = self.metadata_backend() else {
            return Ok(false);
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
                let result =
                    sqlx::query("DELETE FROM tenant_secrets WHERE tenant = $1 AND key_name = $2")
                        .bind(tenant)
                        .bind(key_name)
                        .execute(pool)
                        .await
                        .map_err(|e| format!("failed to delete secret {tenant}/{key_name}: {e}"))?;
                Ok(result.rows_affected() > 0)
            }
            MetadataBackend::Turso(_) => {
                Err("secret deletion is not supported on turso backend yet".to_string())
            }
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("Secret deletion")),
        }
    }

    /// Load all secrets for a tenant from persistence, decrypt, and cache.
    pub async fn load_tenant_secrets(&self, tenant: &str) -> Result<usize, String> {
        let Some(vault) = self.secrets_vault.as_ref() else {
            return Ok(0);
        };
        let Some(backend) = self.metadata_backend() else {
            return Ok(0);
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
                let rows: Vec<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
                    "SELECT key_name, ciphertext, nonce FROM tenant_secrets WHERE tenant = $1",
                )
                .bind(tenant)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("failed to load secrets for tenant {tenant}: {e}"))?;

                let mut count = 0;
                for (key_name, ciphertext, nonce) in &rows {
                    match vault.decrypt(ciphertext, nonce) {
                        Ok(plaintext) => {
                            let value = String::from_utf8(plaintext).map_err(|e| {
                                format!("secret {key_name} is not valid UTF-8: {e}")
                            })?;
                            vault.cache_secret(tenant, key_name, value)?;
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
            MetadataBackend::Turso(_) => {
                Err("secret loading is not supported on turso backend yet".to_string())
            }
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("Secret loading")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use temper_runtime::ActorSystem;
    use temper_store_turso::TursoEventStore;

    use crate::event_store::ServerEventStore;
    use crate::registry::SpecRegistry;
    use crate::secrets_vault::SecretsVault;
    use crate::state::ServerState;

    fn make_state() -> ServerState {
        let system = ActorSystem::new("test-secrets-persistence");
        ServerState::from_registry(system, SpecRegistry::new())
            .with_secrets_vault(SecretsVault::new(&[7u8; 32]))
    }

    #[tokio::test]
    async fn turso_secret_operations_are_explicitly_unsupported() {
        let db_path =
            std::env::temp_dir().join(format!("temper-secrets-{}.db", uuid::Uuid::new_v4()));
        let db_url = format!("file:{}", db_path.display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create local turso db");

        let mut state = make_state();
        state.event_store = Some(Arc::new(ServerEventStore::Turso(store)));

        let vault = state.secrets_vault.as_ref().expect("vault configured");
        let (ciphertext, nonce) = vault.encrypt(b"secret-value").expect("encrypt");

        let put_err = state
            .upsert_secret("tenant-a", "API_KEY", &ciphertext, &nonce)
            .await
            .expect_err("turso secret upsert should fail");
        assert!(put_err.contains("not supported"));

        let del_err = state
            .delete_secret("tenant-a", "API_KEY")
            .await
            .expect_err("turso secret delete should fail");
        assert!(del_err.contains("not supported"));

        let load_err = state
            .load_tenant_secrets("tenant-a")
            .await
            .expect_err("turso secret load should fail");
        assert!(load_err.contains("not supported"));

        let _ = std::fs::remove_file(db_path);
    }
}
