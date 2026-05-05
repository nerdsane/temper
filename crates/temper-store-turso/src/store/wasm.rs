//! WASM module storage and invocation log persistence.

use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{
    TursoEventStore, TursoWasmInvocationRow, TursoWasmModuleMetadataRow, TursoWasmModuleRow,
    write_gate::WritePriority,
};
use crate::TursoWasmInvocationInsert;
use crate::metrics::TursoQueryTimer;

const BUNDLED_REPLACE_UPLOAD_SOURCE: &str = "bundled-replace-upload";

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
        source: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_wasm_module");
        let conn = self.configured_connection().await?;
        // Idempotent + source-aware preservation. See Postgres counterpart for
        // the full reasoning. Plain bundled installs preserve hot uploads, but
        // OS-app reconcile can opt into replacing stale uploads when the
        // bundled WASM digest changed.
        let replace_uploaded_wasm = source == BUNDLED_REPLACE_UPLOAD_SOURCE;
        let persisted_source = if replace_uploaded_wasm {
            "bundled"
        } else {
            source
        };
        let mut existing_rows = conn
            .query(
                "SELECT sha256_hash, source \
                 FROM wasm_modules \
                 WHERE tenant = ?1 AND module_name = ?2",
                params![tenant, module_name],
            )
            .await
            .map_err(storage_error)?;

        if let Some(row) = existing_rows.next().await.map_err(storage_error)? {
            let existing_hash: String = row.get(0).map_err(storage_error)?;
            let existing_source: String = row
                .get::<String>(1)
                .unwrap_or_else(|_| "bundled".to_string());
            if existing_hash == sha256_hash {
                return Ok(());
            }
            if existing_source == "upload" && source != "upload" && !replace_uploaded_wasm {
                tracing::info!(
                    tenant,
                    module_name,
                    existing_hash = %existing_hash,
                    incoming_hash = %sha256_hash,
                    "skipping bundled wasm upsert: existing row is a hot upload"
                );
                return Ok(());
            }
        }
        drop(existing_rows);

        let _write_permit = self
            .acquire_write_permit("turso.upsert_wasm_module", WritePriority::High)
            .await?;
        conn.execute(
            "INSERT INTO wasm_modules (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, datetime('now'), ?6)
             ON CONFLICT (tenant, module_name) DO UPDATE SET
                 wasm_bytes = CASE
                     WHEN wasm_modules.sha256_hash IS NOT excluded.sha256_hash
                     THEN excluded.wasm_bytes ELSE wasm_modules.wasm_bytes END,
                 sha256_hash = CASE
                     WHEN wasm_modules.sha256_hash IS NOT excluded.sha256_hash
                     THEN excluded.sha256_hash ELSE wasm_modules.sha256_hash END,
                 version = CASE
                     WHEN wasm_modules.sha256_hash IS NOT excluded.sha256_hash
                     THEN wasm_modules.version + 1 ELSE wasm_modules.version END,
                 size_bytes = CASE
                     WHEN wasm_modules.sha256_hash IS NOT excluded.sha256_hash
                     THEN excluded.size_bytes ELSE wasm_modules.size_bytes END,
                 updated_at = CASE
                     WHEN wasm_modules.sha256_hash IS NOT excluded.sha256_hash
                     THEN datetime('now') ELSE wasm_modules.updated_at END,
                 source = CASE
                     WHEN wasm_modules.sha256_hash IS NOT excluded.sha256_hash
                     THEN excluded.source ELSE wasm_modules.source END",
            params![tenant, module_name, Vec::<u8>::new(), sha256_hash, wasm_bytes.len() as i64, persisted_source],
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
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source \
                 FROM wasm_modules \
                 WHERE tenant = ?1 AND module_name = ?2",
                params![tenant, module_name],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        Self::row_to_wasm_module(&row).map(Some)
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
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source \
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
                "SELECT tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source \
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

    /// Load WASM module metadata across all tenants without the raw bytes.
    ///
    /// Startup registry recovery should use this instead of loading the full
    /// WASM corpus into memory.
    #[instrument(skip_all, fields(otel.name = "turso.load_wasm_module_metadata_all_tenants"))]
    pub async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleMetadataRow>, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.load_wasm_module_metadata_all_tenants");
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, module_name, sha256_hash, size_bytes, updated_at \
                 FROM wasm_modules \
                 ORDER BY tenant, module_name",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(Self::row_to_wasm_module_metadata(&row)?);
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

    /// Parse a WASM module row from a libsql Row (8 columns).
    fn row_to_wasm_module(row: &libsql::Row) -> Result<TursoWasmModuleRow, PersistenceError> {
        Ok(TursoWasmModuleRow {
            tenant: row.get::<String>(0).map_err(storage_error)?,
            module_name: row.get::<String>(1).map_err(storage_error)?,
            wasm_bytes: row.get::<Vec<u8>>(2).map_err(storage_error)?,
            sha256_hash: row.get::<String>(3).map_err(storage_error)?,
            version: row.get::<i64>(4).map_err(storage_error)? as i32,
            size_bytes: row.get::<i64>(5).map_err(storage_error)? as i32,
            updated_at: row.get::<String>(6).map_err(storage_error)?,
            source: row
                .get::<String>(7)
                .unwrap_or_else(|_| "bundled".to_string()),
        })
    }

    /// Parse a WASM module metadata row from a libsql Row (5 columns).
    fn row_to_wasm_module_metadata(
        row: &libsql::Row,
    ) -> Result<TursoWasmModuleMetadataRow, PersistenceError> {
        Ok(TursoWasmModuleMetadataRow {
            tenant: row.get::<String>(0).map_err(storage_error)?,
            module_name: row.get::<String>(1).map_err(storage_error)?,
            sha256_hash: row.get::<String>(2).map_err(storage_error)?,
            size_bytes: row.get::<i64>(3).map_err(storage_error)? as i32,
            updated_at: row.get::<String>(4).map_err(storage_error)?,
        })
    }
}
