//! WASM module storage and invocation log persistence.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, TursoWasmInvocationRow, TursoWasmModuleRow};
use crate::TursoWasmInvocationInsert;
use crate::metrics::TursoQueryTimer;

impl TursoEventStore {
    /// Upsert a WASM module binary for a tenant.
    ///
    /// If the module already exists, its version is incremented and the binary
    /// is replaced. Returns the SHA-256 hash of the stored module.
    #[instrument(skip_all, fields(tenant, module_name, otel.name = "turso.upsert_wasm_module"))]
    pub async fn upsert_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
        wasm_bytes: &[u8],
        sha256_hash: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_wasm_module");
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO wasm_modules (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, datetime('now'))
             ON CONFLICT (tenant, module_name) DO UPDATE SET
                 wasm_bytes = excluded.wasm_bytes,
                 sha256_hash = excluded.sha256_hash,
                 version = wasm_modules.version + 1,
                 size_bytes = excluded.size_bytes,
                 updated_at = datetime('now')",
            params![tenant, module_name, wasm_bytes.to_vec(), sha256_hash, wasm_bytes.len() as i64],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load a WASM module by tenant and name.
    #[instrument(skip_all, fields(tenant, module_name, otel.name = "turso.load_wasm_module"))]
    pub async fn load_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<Option<TursoWasmModuleRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_wasm_module");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at \
                 FROM wasm_modules \
                 WHERE tenant = ?1 AND module_name = ?2",
                params![tenant, module_name],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        Ok(Some(Self::row_to_wasm_module(&row)?))
    }

    /// Load all WASM modules for a tenant.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.load_all_wasm_modules"))]
    pub async fn load_all_wasm_modules(
        &self,
        tenant: &str,
    ) -> Result<Vec<TursoWasmModuleRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_all_wasm_modules");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at \
                 FROM wasm_modules \
                 WHERE tenant = ?1 \
                 ORDER BY module_name",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_wasm_module(&row)?);
        }
        Ok(out)
    }

    /// Load all WASM modules across all tenants (for startup recovery).
    #[instrument(skip_all, fields(otel.name = "turso.load_wasm_modules_all_tenants"))]
    pub async fn load_wasm_modules_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_wasm_modules_all_tenants");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at \
                 FROM wasm_modules \
                 ORDER BY tenant, module_name",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_wasm_module(&row)?);
        }
        Ok(out)
    }

    /// Persist a WASM invocation log entry.
    #[instrument(skip_all, fields(otel.name = "turso.persist_wasm_invocation"))]
    pub async fn persist_wasm_invocation(
        &self,
        entry: &TursoWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.persist_wasm_invocation");
        let conn = self.configured_connection().await?;
        let success_val: i64 = if entry.success { 1 } else { 0 };
        let duration_val: i64 = entry.duration_ms as i64;
        conn.execute(
            "INSERT INTO wasm_invocation_logs \
             (tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                entry.tenant,
                entry.entity_type,
                entry.entity_id,
                entry.module_name,
                entry.trigger_action,
                entry.callback_action,
                success_val,
                entry.error,
                duration_val,
                entry.created_at
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load recent WASM invocation log entries (newest first, up to `limit`).
    #[instrument(skip_all, fields(otel.name = "turso.load_recent_wasm_invocations"))]
    pub async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoWasmInvocationRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_recent_wasm_invocations");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, entity_id, module_name, trigger_action, \
                        callback_action, success, error, duration_ms, created_at \
                 FROM wasm_invocation_logs \
                 ORDER BY created_at DESC \
                 LIMIT ?1",
                params![limit],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoWasmInvocationRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                entity_type: row.get::<String>(1).map_err(storage_error)?,
                entity_id: row.get::<String>(2).map_err(storage_error)?,
                module_name: row.get::<String>(3).map_err(storage_error)?,
                trigger_action: row.get::<String>(4).map_err(storage_error)?,
                callback_action: row.get::<Option<String>>(5).map_err(storage_error)?,
                success: row.get::<i64>(6).map_err(storage_error)? != 0,
                error: row.get::<Option<String>>(7).map_err(storage_error)?,
                duration_ms: row.get::<i64>(8).map_err(storage_error)? as u64,
                created_at: row.get::<String>(9).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Delete a WASM module.
    #[instrument(skip_all, fields(tenant, module_name, otel.name = "turso.delete_wasm_module"))]
    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_wasm_module");
        let conn = self.configured_connection().await?;
        let affected = conn
            .execute(
                "DELETE FROM wasm_modules WHERE tenant = ?1 AND module_name = ?2",
                params![tenant, module_name],
            )
            .await
            .map_err(storage_error)?;
        Ok(affected > 0)
    }

    /// Parse a WASM module row from a libsql Row (7 columns).
    fn row_to_wasm_module(row: &libsql::Row) -> Result<TursoWasmModuleRow, PersistenceError> {
        Ok(TursoWasmModuleRow {
            tenant: row.get::<String>(0).map_err(storage_error)?,
            module_name: row.get::<String>(1).map_err(storage_error)?,
            wasm_bytes: row.get::<Vec<u8>>(2).map_err(storage_error)?,
            sha256_hash: row.get::<String>(3).map_err(storage_error)?,
            version: row.get::<i64>(4).map_err(storage_error)? as i32,
            size_bytes: row.get::<i64>(5).map_err(storage_error)? as i32,
            updated_at: row.get::<String>(6).map_err(storage_error)?,
        })
    }
}
