//! Persistence methods for ServerState (Postgres, Turso, Redis backends).

use sqlx::PgPool;
use temper_runtime::scheduler::sim_now;
use temper_store_turso::{TursoEventStore, TursoWasmInvocationInsert};

use super::ServerState;
use super::wasm_invocation_log::WasmInvocationEntry;

/// Owned metadata backend for tenant-scoped operations.
///
/// `turso_for_tenant()` returns an owned `TursoEventStore` (Arc-based,
/// clone is cheap), so tenant-scoped operations use this owned variant.
pub(crate) enum TenantMetadataBackend {
    Postgres(PgPool),
    Turso(TursoEventStore),
    Redis,
}

mod logs_and_secrets;
mod spec_metadata;

impl ServerState {
    fn redis_ephemeral_error(operation: &str) -> String {
        format!(
            "{operation} is not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
        )
    }

    /// Return a tenant-scoped metadata backend.
    ///
    /// In TenantRouted mode, routes to the per-tenant database.
    /// In single-DB Turso mode, returns the shared store.
    /// In Postgres mode, returns the shared pool (RLS handles isolation).
    pub(crate) async fn metadata_backend_for_tenant(
        &self,
        tenant: &str,
    ) -> Option<TenantMetadataBackend> {
        let store = self.event_store.as_ref()?;
        if let Some(pool) = store.postgres_pool() {
            return Some(TenantMetadataBackend::Postgres(pool.clone()));
        }
        if let Some(turso) = store.turso_for_tenant(tenant).await {
            return Some(TenantMetadataBackend::Turso(turso));
        }
        if store.redis_store().is_some() {
            Some(TenantMetadataBackend::Redis)
        } else {
            None
        }
    }

    /// Upsert a WASM module in the persistence backend (Postgres or Turso).
    pub async fn upsert_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
        wasm_bytes: &[u8],
        sha256_hash: &str,
    ) -> Result<(), String> {
        let Some(backend) = self.metadata_backend_for_tenant(tenant).await else {
            return Ok(());
        };

        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO wasm_modules (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at) \
                     VALUES ($1, $2, $3, $4, 1, $5, now()) \
                     ON CONFLICT (tenant, module_name) DO UPDATE SET \
                         wasm_bytes = EXCLUDED.wasm_bytes, \
                         sha256_hash = EXCLUDED.sha256_hash, \
                         version = wasm_modules.version + 1, \
                         size_bytes = EXCLUDED.size_bytes, \
                         updated_at = now()",
                )
                .bind(tenant)
                .bind(module_name)
                .bind(wasm_bytes)
                .bind(sha256_hash)
                .bind(wasm_bytes.len() as i32)
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| format!("failed to upsert WASM module {tenant}/{module_name}: {e}"))
            }
            TenantMetadataBackend::Turso(turso) => turso
                .upsert_wasm_module(tenant, module_name, wasm_bytes, sha256_hash)
                .await
                .map_err(|e| {
                    format!("failed to upsert WASM module {tenant}/{module_name} in turso: {e}")
                }),
            TenantMetadataBackend::Redis => Err(Self::redis_ephemeral_error("WASM module persistence")),
        }
    }

    /// Delete a WASM module from persistence.
    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, String> {
        let Some(backend) = self.metadata_backend_for_tenant(tenant).await else {
            return Ok(false);
        };

        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                let result =
                    sqlx::query("DELETE FROM wasm_modules WHERE tenant = $1 AND module_name = $2")
                        .bind(tenant)
                        .bind(module_name)
                        .execute(&pool)
                        .await
                        .map_err(|e| {
                            format!("failed to delete WASM module {tenant}/{module_name}: {e}")
                        })?;
                Ok(result.rows_affected() > 0)
            }
            TenantMetadataBackend::Turso(turso) => turso
                .delete_wasm_module(tenant, module_name)
                .await
                .map_err(|e| {
                    format!("failed to delete WASM module {tenant}/{module_name} in turso: {e}")
                }),
            TenantMetadataBackend::Redis => {
                Err(Self::redis_ephemeral_error("WASM module deletion"))
            }
        }
    }

    /// Persist a WASM invocation log entry (Postgres or Turso).
    ///
    /// Fire-and-forget — callers should not block the dispatch path on this.
    pub async fn persist_wasm_invocation(&self, entry: &WasmInvocationEntry) -> Result<(), String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(());
        };

        // Postgres path.
        if let Some(pool) = store.postgres_pool() {
            let created_at = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| sim_now());
            sqlx::query(
                "INSERT INTO wasm_invocation_logs \
                 (tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(&entry.tenant)
            .bind(&entry.entity_type)
            .bind(&entry.entity_id)
            .bind(&entry.module_name)
            .bind(&entry.trigger_action)
            .bind(entry.callback_action.as_deref())
            .bind(entry.success)
            .bind(entry.error.as_deref())
            .bind(entry.duration_ms as i64)
            .bind(created_at)
            .execute(pool)
            .await
            .map_err(|e| {
                format!(
                    "failed to persist WASM invocation for {}/{}: {e}",
                    entry.tenant, entry.module_name
                )
            })?;
            return Ok(());
        }

        // Turso path (tenant-routed).
        if let Some(turso) = store.turso_for_tenant(&entry.tenant).await {
            turso
                .persist_wasm_invocation(&TursoWasmInvocationInsert {
                    tenant: &entry.tenant,
                    entity_type: &entry.entity_type,
                    entity_id: &entry.entity_id,
                    module_name: &entry.module_name,
                    trigger_action: &entry.trigger_action,
                    callback_action: entry.callback_action.as_deref(),
                    success: entry.success,
                    error: entry.error.as_deref(),
                    duration_ms: entry.duration_ms,
                    created_at: &entry.timestamp,
                })
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist WASM invocation for {}/{} in turso: {e}",
                        entry.tenant, entry.module_name
                    )
                })?;
            return Ok(());
        }

        Ok(())
    }

    /// Load all WASM modules from the persistence backend and register them.
    ///
    /// For each module, compiles the bytes via `WasmEngine::compile_and_cache()`
    /// and registers in the `WasmModuleRegistry`.
    pub async fn load_wasm_modules(&self) -> Result<usize, String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(0);
        };

        let mut recovered = 0usize;

        // Postgres path.
        if let Some(pool) = store.postgres_pool() {
            let rows: Vec<(String, String, Vec<u8>, String)> = sqlx::query_as(
                "SELECT tenant, module_name, wasm_bytes, sha256_hash FROM wasm_modules ORDER BY tenant, module_name",
            )
            .fetch_all(pool)
            .await
            .map_err(|e| format!("failed to load WASM modules from postgres: {e}"))?;

            for (tenant, module_name, wasm_bytes, _stored_hash) in rows {
                match self.wasm_engine.compile_and_cache(&wasm_bytes) {
                    Ok(hash) => {
                        let tenant_id = temper_runtime::tenant::TenantId::new(&tenant);
                        let mut wasm_reg = self.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                        wasm_reg.register(&tenant_id, &module_name, &hash);
                        recovered += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            tenant = %tenant,
                            module = %module_name,
                            error = %e,
                            "failed to compile recovered WASM module, skipping"
                        );
                    }
                }
            }
            return Ok(recovered);
        }

        // Turso path (single-DB).
        if let Some(turso) = store.platform_turso_store() {
            let rows = turso
                .load_wasm_modules_all_tenants()
                .await
                .map_err(|e| format!("failed to load WASM modules from turso: {e}"))?;

            for row in rows {
                match self.wasm_engine.compile_and_cache(&row.wasm_bytes) {
                    Ok(hash) => {
                        let tenant_id = temper_runtime::tenant::TenantId::new(&row.tenant);
                        let mut wasm_reg = self.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                        wasm_reg.register(&tenant_id, &row.module_name, &hash);
                        recovered += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            tenant = %row.tenant,
                            module = %row.module_name,
                            error = %e,
                            "failed to compile recovered WASM module, skipping"
                        );
                    }
                }
            }
            return Ok(recovered);
        }

        // Turso tenant-routed path: load from each connected tenant + platform.
        if let Some(router) = store.tenant_router() {
            // Load from platform store (system modules).
            if let Ok(rows) = router
                .platform_store()
                .load_wasm_modules_all_tenants()
                .await
            {
                for row in rows {
                    if let Ok(hash) = self.wasm_engine.compile_and_cache(&row.wasm_bytes) {
                        let tenant_id = temper_runtime::tenant::TenantId::new(&row.tenant);
                        let mut wasm_reg = self.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                        wasm_reg.register(&tenant_id, &row.module_name, &hash);
                        recovered += 1;
                    }
                }
            }
            // Load from each tenant store.
            for tid in router.connected_tenants().await {
                let Ok(turso) = router.store_for_tenant(&tid).await else {
                    continue;
                };
                let Ok(rows) = turso.load_wasm_modules_all_tenants().await else {
                    continue;
                };
                for row in rows {
                    match self.wasm_engine.compile_and_cache(&row.wasm_bytes) {
                        Ok(hash) => {
                            let tenant_id = temper_runtime::tenant::TenantId::new(&row.tenant);
                            let mut wasm_reg = self.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                            wasm_reg.register(&tenant_id, &row.module_name, &hash);
                            recovered += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                tenant = %row.tenant,
                                module = %row.module_name,
                                error = %e,
                                "failed to compile recovered WASM module, skipping"
                            );
                        }
                    }
                }
            }
            return Ok(recovered);
        }

        Ok(0)
    }
}
