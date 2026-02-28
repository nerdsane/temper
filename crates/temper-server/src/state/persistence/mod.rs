//! Persistence methods for ServerState (Postgres, Turso, Redis backends).

use sqlx::PgPool;
use temper_runtime::scheduler::sim_now;
use temper_store_turso::{TursoEventStore, TursoWasmInvocationInsert};

use super::wasm_invocation_log::WasmInvocationEntry;
use super::{DESIGN_TIME_LOG_CAPACITY, DesignTimeEvent, ServerState};

enum MetadataBackend<'a> {
    Postgres(&'a PgPool),
    Turso(&'a TursoEventStore),
    Redis,
}

/// Row type for WASM invocation log queries (avoids clippy::type_complexity).
type WasmInvocationRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    bool,
    Option<String>,
    i64,
    chrono::DateTime<chrono::Utc>,
);

mod logs_and_secrets;
mod spec_metadata;

impl ServerState {
    fn redis_ephemeral_error(operation: &str) -> String {
        format!(
            "{operation} is not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
        )
    }

    fn metadata_backend(&self) -> Option<MetadataBackend<'_>> {
        let store = self.event_store.as_ref()?;
        if let Some(pool) = store.postgres_pool() {
            return Some(MetadataBackend::Postgres(pool));
        }
        if let Some(turso) = store.turso_store() {
            return Some(MetadataBackend::Turso(turso));
        }
        if store.redis_store().is_some() {
            Some(MetadataBackend::Redis)
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
        let Some(backend) = self.metadata_backend() else {
            return Ok(());
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
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
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(|e| format!("failed to upsert WASM module {tenant}/{module_name}: {e}"))
            }
            MetadataBackend::Turso(turso) => turso
                .upsert_wasm_module(tenant, module_name, wasm_bytes, sha256_hash)
                .await
                .map_err(|e| {
                    format!("failed to upsert WASM module {tenant}/{module_name} in turso: {e}")
                }),
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("WASM module persistence")),
        }
    }

    /// Delete a WASM module from persistence.
    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, String> {
        let Some(backend) = self.metadata_backend() else {
            return Ok(false);
        };

        match backend {
            MetadataBackend::Postgres(pool) => {
                let result =
                    sqlx::query("DELETE FROM wasm_modules WHERE tenant = $1 AND module_name = $2")
                        .bind(tenant)
                        .bind(module_name)
                        .execute(pool)
                        .await
                        .map_err(|e| {
                            format!("failed to delete WASM module {tenant}/{module_name}: {e}")
                        })?;
                Ok(result.rows_affected() > 0)
            }
            MetadataBackend::Turso(turso) => turso
                .delete_wasm_module(tenant, module_name)
                .await
                .map_err(|e| {
                    format!("failed to delete WASM module {tenant}/{module_name} in turso: {e}")
                }),
            MetadataBackend::Redis => Err(Self::redis_ephemeral_error("WASM module deletion")),
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

        // Turso path.
        if let Some(turso) = store.turso_store() {
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

        // Turso path.
        if let Some(turso) = store.turso_store() {
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

        Ok(0)
    }

    /// Load recent WASM invocation entries from the persistence backend
    /// into the in-memory bounded log.
    pub async fn load_recent_wasm_invocations(&self, limit: usize) -> Result<usize, String> {
        let Some(store) = self.event_store.as_ref() else {
            return Ok(0);
        };

        // Postgres path.
        if let Some(pool) = store.postgres_pool() {
            let rows: Vec<WasmInvocationRow> = sqlx::query_as(
                "SELECT tenant, entity_type, entity_id, module_name, trigger_action, \
                        callback_action, success, error, duration_ms, created_at \
                 FROM wasm_invocation_logs \
                 ORDER BY created_at DESC \
                 LIMIT $1",
            )
            .bind(limit as i64)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("failed to load WASM invocations from postgres: {e}"))?;

            let count = rows.len();
            if let Ok(mut log) = self.wasm_invocation_log.write() {
                // Insert oldest-first (rows are newest-first from query).
                for (
                    tenant,
                    entity_type,
                    entity_id,
                    module_name,
                    trigger_action,
                    callback_action,
                    success,
                    error,
                    duration_ms,
                    created_at,
                ) in rows.into_iter().rev()
                {
                    log.push(WasmInvocationEntry {
                        timestamp: created_at.to_rfc3339(),
                        tenant,
                        entity_type,
                        entity_id,
                        module_name,
                        trigger_action,
                        callback_action,
                        success,
                        error,
                        duration_ms: duration_ms as u64,
                        authz_denied: None,
                    });
                }
            }
            return Ok(count);
        }

        // Turso path.
        if let Some(turso) = store.turso_store() {
            let rows = turso
                .load_recent_wasm_invocations(limit as i64)
                .await
                .map_err(|e| format!("failed to load WASM invocations from turso: {e}"))?;

            let count = rows.len();
            if let Ok(mut log) = self.wasm_invocation_log.write() {
                // Rows come newest-first, insert oldest-first.
                for row in rows.into_iter().rev() {
                    log.push(WasmInvocationEntry {
                        timestamp: row.created_at,
                        tenant: row.tenant,
                        entity_type: row.entity_type,
                        entity_id: row.entity_id,
                        module_name: row.module_name,
                        trigger_action: row.trigger_action,
                        callback_action: row.callback_action,
                        success: row.success,
                        error: row.error,
                        duration_ms: row.duration_ms,
                        authz_denied: None,
                    });
                }
            }
            return Ok(count);
        }

        Ok(0)
    }

    /// Append to in-memory design-time log with bounded capacity.
    pub fn push_design_time_event(&self, event: DesignTimeEvent) {
        if let Ok(mut log) = self.design_time_log.write() {
            if log.len() >= DESIGN_TIME_LOG_CAPACITY {
                // Keep the newest events; evict oldest one.
                let _ = log.remove(0);
            }
            log.push(event);
        }
    }
}
