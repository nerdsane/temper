//! WASM module storage and invocation logs.

use sqlx::Row;
use temper_runtime::persistence::PersistenceError;

use super::{parse_rfc3339, storage_error};
use crate::PostgresEventStore;

const BUNDLED_REPLACE_UPLOAD_SOURCE: &str = "bundled-replace-upload";

#[derive(Debug, Clone)]
pub struct PostgresWasmModuleRow {
    pub tenant: String,
    pub module_name: String,
    pub wasm_bytes: Vec<u8>,
    pub sha256_hash: String,
    /// Provenance: `"bundled"` (install pipeline) or `"upload"` (hot upload).
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct PostgresWasmModuleMetadataRow {
    pub tenant: String,
    pub module_name: String,
    pub sha256_hash: String,
    pub size_bytes: i32,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug)]
pub struct PostgresWasmInvocationInsert<'a> {
    pub tenant: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub module_name: &'a str,
    pub trigger_action: &'a str,
    pub callback_action: Option<&'a str>,
    pub success: bool,
    pub error: Option<&'a str>,
    pub duration_ms: u64,
    pub created_at: &'a str,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PostgresWasmInvocationRow {
    pub tenant: String,
    pub entity_type: String,
    pub entity_id: String,
    pub module_name: String,
    pub trigger_action: String,
    pub callback_action: Option<String>,
    pub success: bool,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub created_at: String,
}

impl PostgresEventStore {
    pub async fn load_all_wasm_modules(
        &self,
        tenant: &str,
    ) -> Result<Vec<PostgresWasmModuleRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, module_name, wasm_bytes, sha256_hash, source \
             FROM wasm_modules WHERE tenant = $1 ORDER BY module_name",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_wasm_module).collect())
    }

    pub async fn load_wasm_modules_all_tenants(
        &self,
    ) -> Result<Vec<PostgresWasmModuleRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, module_name, wasm_bytes, sha256_hash, source \
             FROM wasm_modules ORDER BY tenant, module_name",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_wasm_module).collect())
    }

    pub async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
        source: &str,
    ) -> Result<(), PersistenceError> {
        // Idempotent on hash + source-aware preservation:
        //   - source='upload' callers (hot upload via the API) overwrite anything
        //     so iterative testing works.
        //   - source='bundled' callers (the os-apps install pipeline) only
        //     overwrite existing 'bundled' rows. They preserve hot uploads
        //     across same-bundle restarts.
        //   - source='bundled-replace-upload' is an internal reconcile mode:
        //     persist the row back as 'bundled' while replacing stale uploads
        //     after the installed app's bundled WASM digest changed.
        let replace_uploaded_wasm = source == BUNDLED_REPLACE_UPLOAD_SOURCE;
        let persisted_source = if replace_uploaded_wasm {
            "bundled"
        } else {
            source
        };
        crate::dbm::postgres_query!(
            "INSERT INTO wasm_modules \
             (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source) \
             VALUES ($1, $2, $3, $4, 1, $5, now(), $6) \
             ON CONFLICT (tenant, module_name) DO UPDATE SET \
                 wasm_bytes = EXCLUDED.wasm_bytes, \
                 sha256_hash = EXCLUDED.sha256_hash, \
                 version = wasm_modules.version + 1, \
                 size_bytes = EXCLUDED.size_bytes, \
                 updated_at = now(), \
                 source = EXCLUDED.source \
             WHERE wasm_modules.sha256_hash IS DISTINCT FROM EXCLUDED.sha256_hash \
                AND ($7 OR EXCLUDED.source = 'upload' OR wasm_modules.source = 'bundled')",
        )
        .bind(tenant)
        .bind(name)
        .bind(bytes)
        .bind(hash)
        .bind(bytes.len() as i32)
        .bind(persisted_source)
        .bind(replace_uploaded_wasm)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn load_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<Option<PostgresWasmModuleRow>, PersistenceError> {
        let row = crate::dbm::postgres_query!(
            "SELECT tenant, module_name, wasm_bytes, sha256_hash, source \
             FROM wasm_modules WHERE tenant = $1 AND module_name = $2",
        )
        .bind(tenant)
        .bind(module_name)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(row_to_wasm_module))
    }

    pub async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<PostgresWasmModuleMetadataRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, module_name, sha256_hash, size_bytes, updated_at \
             FROM wasm_modules ORDER BY tenant, module_name",
        )
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_wasm_module_metadata).collect())
    }

    pub async fn persist_wasm_invocation(
        &self,
        entry: &PostgresWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        let created_at = parse_rfc3339(entry.created_at)?;
        crate::dbm::postgres_query!(
            "INSERT INTO wasm_invocation_logs \
             (tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(entry.tenant)
        .bind(entry.entity_type)
        .bind(entry.entity_id)
        .bind(entry.module_name)
        .bind(entry.trigger_action)
        .bind(entry.callback_action)
        .bind(entry.success)
        .bind(entry.error)
        .bind(entry.duration_ms as i64)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    pub async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<PostgresWasmInvocationRow>, PersistenceError> {
        let rows = crate::dbm::postgres_query!(
            "SELECT tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at \
             FROM wasm_invocation_logs \
             ORDER BY created_at DESC \
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows.into_iter().map(row_to_wasm_invocation).collect())
    }

    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        let result = crate::dbm::postgres_query!(
            "DELETE FROM wasm_modules WHERE tenant = $1 AND module_name = $2"
        )
        .bind(tenant)
        .bind(module_name)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }
}

fn row_to_wasm_module(row: sqlx::postgres::PgRow) -> PostgresWasmModuleRow {
    let source: Option<String> = row.try_get("source").ok();
    PostgresWasmModuleRow {
        tenant: row.get("tenant"),
        module_name: row.get("module_name"),
        wasm_bytes: row.get("wasm_bytes"),
        sha256_hash: row.get("sha256_hash"),
        source: source.unwrap_or_else(|| "bundled".to_string()),
    }
}

fn row_to_wasm_module_metadata(row: sqlx::postgres::PgRow) -> PostgresWasmModuleMetadataRow {
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    PostgresWasmModuleMetadataRow {
        tenant: row.get("tenant"),
        module_name: row.get("module_name"),
        sha256_hash: row.get("sha256_hash"),
        size_bytes: row.get("size_bytes"),
        updated_at: updated_at.to_rfc3339(),
    }
}

fn row_to_wasm_invocation(row: sqlx::postgres::PgRow) -> PostgresWasmInvocationRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let duration_ms: i64 = row.get("duration_ms");
    PostgresWasmInvocationRow {
        tenant: row.get("tenant"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        module_name: row.get("module_name"),
        trigger_action: row.get("trigger_action"),
        callback_action: row.get("callback_action"),
        success: row.get("success"),
        error: row.get("error"),
        duration_ms: duration_ms as u64,
        created_at: created_at.to_rfc3339(),
    }
}
