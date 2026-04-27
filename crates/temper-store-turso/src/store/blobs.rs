//! Turso-backed blob storage for TemperFS `$value` endpoints.
//!
//! Content-addressed storage: blobs are keyed by `{bucket}/{content_hash}`.
//! This provides persistent local blob storage so the blob_adapter WASM module
//! can upload/download via HTTP without requiring external S3/R2.

use crate::TursoEventStore;
use libsql::params;
use std::time::Duration;

use super::write_gate::WritePriority;

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
    ///
    /// Writes with `expires_at = NULL` (permanent). Use `put_blob_with_ttl`
    /// for opt-in expiration. See ADR-0047.
    pub async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        self.put_blob_with_ttl(key, data, None).await
    }

    /// Store a blob with an optional TTL.
    ///
    /// `ttl = None` writes `expires_at = NULL` (permanent; same as `put_blob`).
    /// `ttl = Some(d)` writes `expires_at = datetime('now', '+N seconds')` so
    /// `sweep_expired_blobs` can reclaim the row once the deadline passes.
    ///
    /// Content-addressed dedup via `INSERT OR IGNORE` preserves the first
    /// writer's `expires_at`. A caller that later re-puts the same bytes with
    /// a different TTL does not override the existing row's expiry; this is
    /// correct semantics for "if any writer considered this transient, the
    /// storage contract is transient." See ADR-0047.
    pub async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let ttl_seconds = ttl.map(|d| d.as_secs() as i64);
        for attempt in 1..=BLOB_STORE_ATTEMPTS {
            let _write_permit = self
                .acquire_write_permit("turso.put_blob", WritePriority::Low)
                .await
                .map_err(|e| e.to_string())?;
            let conn = self
                .configured_connection()
                .await
                .map_err(|e| e.to_string())?;
            let result = match ttl_seconds {
                Some(secs) => {
                    let expr = format!("datetime('now', '+{secs} seconds')");
                    let sql = format!(
                        "INSERT OR IGNORE INTO blobs (blob_key, data, size_bytes, expires_at) \
                         VALUES (?1, ?2, ?3, {expr})"
                    );
                    conn.execute(&sql, params![key, data.to_vec(), data.len() as i64])
                        .await
                }
                None => {
                    conn.execute(
                        "INSERT OR IGNORE INTO blobs (blob_key, data, size_bytes) VALUES (?1, ?2, ?3)",
                        params![key, data.to_vec(), data.len() as i64],
                    )
                    .await
                }
            };
            match result {
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

    /// Delete up to `max_rows` expired blob rows. Returns the number of rows
    /// actually deleted. Callers loop until the count is `< max_rows`.
    ///
    /// Predicate: `expires_at IS NOT NULL AND expires_at < datetime('now')`.
    /// Rows with `expires_at = NULL` (the default) are never touched. See
    /// ADR-0047.
    pub async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String> {
        let _write_permit = self
            .acquire_write_permit("turso.sweep_expired_blobs", WritePriority::Low)
            .await
            .map_err(|e| e.to_string())?;
        let conn = self
            .configured_connection()
            .await
            .map_err(|e| e.to_string())?;
        let sql = format!(
            "DELETE FROM blobs \
             WHERE blob_key IN ( \
                 SELECT blob_key FROM blobs \
                 WHERE expires_at IS NOT NULL AND expires_at < datetime('now') \
                 LIMIT {max_rows} \
             )"
        );
        let affected = conn
            .execute(&sql, ())
            .await
            .map_err(|e| format!("blob sweep failed: {e}"))?;
        Ok(affected)
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
