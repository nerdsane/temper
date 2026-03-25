//! Turso-backed blob storage for TemperFS `$value` endpoints.
//!
//! Content-addressed storage: blobs are keyed by `{bucket}/{content_hash}`.
//! This provides persistent local blob storage so the blob_adapter WASM module
//! can upload/download via HTTP without requiring external S3/R2.

use crate::TursoEventStore;
use libsql::params;

impl TursoEventStore {
    /// Store a blob by key (content-addressed path like `temper-fs/sha256:abc...`).
    pub async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        let conn = self.connection().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT OR REPLACE INTO blobs (blob_key, data, size_bytes) VALUES (?1, ?2, ?3)",
            params![key, data.to_vec(), data.len() as i64],
        )
        .await
        .map_err(|e| format!("blob put failed: {e}"))?;
        Ok(())
    }

    /// Retrieve a blob by key. Returns `None` if not found.
    pub async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let conn = self.connection().map_err(|e| e.to_string())?;
        let mut rows = conn
            .query("SELECT data FROM blobs WHERE blob_key = ?1", params![key])
            .await
            .map_err(|e| format!("blob get failed: {e}"))?;

        match rows.next().await {
            Ok(Some(row)) => {
                let data: Vec<u8> = row
                    .get_value(0)
                    .map_err(|e| format!("blob read failed: {e}"))
                    .and_then(|v| match v {
                        libsql::Value::Blob(b) => Ok(b),
                        _ => Err("blob column is not BLOB type".to_string()),
                    })?;
                Ok(Some(data))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(format!("blob query failed: {e}")),
        }
    }
}
