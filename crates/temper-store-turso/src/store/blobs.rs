//! Turso-backed blob storage for TemperFS `$value` endpoints.
//!
//! Content-addressed storage: blobs are keyed by `{bucket}/{content_hash}`.
//! This provides persistent local blob storage so the blob_adapter WASM module
//! can upload/download via HTTP without requiring external S3/R2.

use crate::TursoEventStore;
use libsql::params;
use std::time::Duration;

const BLOB_STORE_ATTEMPTS: usize = 6;

fn is_blob_lock_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("database is locked") || normalized.contains("database table is locked")
}

fn blob_retry_backoff(attempt: usize) -> Duration {
    let shift = u32::try_from(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(5);
    Duration::from_millis(25_u64.saturating_mul(1_u64 << shift))
}

impl TursoEventStore {
    /// Store a blob by key (content-addressed path like `temper-fs/sha256:abc...`).
    pub async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        for attempt in 1..=BLOB_STORE_ATTEMPTS {
            let conn = self
                .configured_connection()
                .await
                .map_err(|e| e.to_string())?;
            match conn
                .execute(
                    "INSERT OR IGNORE INTO blobs (blob_key, data, size_bytes) VALUES (?1, ?2, ?3)",
                    params![key, data.to_vec(), data.len() as i64],
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) => {
                    let message = error.to_string();
                    if attempt == BLOB_STORE_ATTEMPTS || !is_blob_lock_error(&message) {
                        return Err(format!("blob put failed: {error}"));
                    }
                    let backoff = blob_retry_backoff(attempt);
                    tracing::warn!(
                        path = %key,
                        attempt,
                        max_attempts = BLOB_STORE_ATTEMPTS,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %message,
                        "retrying blob put after transient SQLite lock"
                    );
                    tokio::time::sleep(backoff).await; // determinism-ok: storage backoff for transient SQLite lock contention
                }
            }
        }

        Err("blob put failed: exhausted retry budget".to_string())
    }

    /// Retrieve a blob by key. Returns `None` if not found.
    pub async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        for attempt in 1..=BLOB_STORE_ATTEMPTS {
            let conn = self
                .configured_connection()
                .await
                .map_err(|e| e.to_string())?;
            let mut rows = match conn
                .query("SELECT data FROM blobs WHERE blob_key = ?1", params![key])
                .await
            {
                Ok(rows) => rows,
                Err(error) => {
                    let message = error.to_string();
                    if attempt == BLOB_STORE_ATTEMPTS || !is_blob_lock_error(&message) {
                        return Err(format!("blob get failed: {error}"));
                    }
                    let backoff = blob_retry_backoff(attempt);
                    tracing::warn!(
                        path = %key,
                        attempt,
                        max_attempts = BLOB_STORE_ATTEMPTS,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %message,
                        "retrying blob get after transient SQLite lock"
                    );
                    tokio::time::sleep(backoff).await; // determinism-ok: storage backoff for transient SQLite lock contention
                    continue;
                }
            };

            return match rows.next().await {
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
                Err(error) => {
                    let message = error.to_string();
                    if attempt == BLOB_STORE_ATTEMPTS || !is_blob_lock_error(&message) {
                        Err(format!("blob query failed: {error}"))
                    } else {
                        let backoff = blob_retry_backoff(attempt);
                        tracing::warn!(
                            path = %key,
                            attempt,
                            max_attempts = BLOB_STORE_ATTEMPTS,
                            backoff_ms = backoff.as_millis() as u64,
                            error = %message,
                            "retrying blob row fetch after transient SQLite lock"
                        );
                        tokio::time::sleep(backoff).await; // determinism-ok: storage backoff for transient SQLite lock contention
                        continue;
                    }
                }
            };
        }

        Err("blob get failed: exhausted retry budget".to_string())
    }
}
