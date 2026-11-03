//! Bounded reads of legacy blob values. SQL substr alone is insufficient for
//! embedded Turso: its evaluator materializes the source BLOB before slicing.
use crate::{TursoEventStore, driver::params};

impl TursoEventStore {
    /// Read at most `max_bytes` at a zero-based offset, with the actual total
    /// byte length. Local files use SQLite's incremental BLOB API, never a
    /// SELECT of the value; remote queries return only the requested slice.
    pub async fn get_blob_chunk(
        &self,
        key: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<Option<(u64, Vec<u8>)>, String> {
        let start = i64::try_from(offset)
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| "blob offset exceeds i64".to_string())?;
        let count = i64::try_from(max_bytes).map_err(|_| "blob chunk exceeds i64".to_string())?;
        if let Some(path) = self.blob_read_path.clone() {
            let key = key.to_owned();
            // Keep the driver's connection lease while reading. A synced
            // database's checkpoint must not replace its file during the read.
            let lease = self
                .configured_connection()
                .await
                .map_err(|e| e.to_string())?;
            // Keep a live, index-only Turso read as well. The embedded engine
            // must not recycle WAL frames while SQLite is reading them; the
            // lease alone protects only the sync adapter's explicit checkpoint.
            let mut snapshot = lease
                .query(
                    "SELECT blob_key FROM blobs WHERE blob_key = ?1",
                    params![key.as_str()],
                )
                .await
                .map_err(|e| e.to_string())?;
            if snapshot.next().await.map_err(|e| e.to_string())?.is_none() {
                return Ok(None);
            }
            return tokio::task::spawn_blocking(move || {
                let _lease = lease;
                let _snapshot = snapshot;
                let conn = rusqlite::Connection::open_with_flags(
                    path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .map_err(|e| e.to_string())?;
                conn.busy_timeout(std::time::Duration::from_secs(5))
                    .map_err(|e| e.to_string())?;
                conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
                use rusqlite::OptionalExtension as _;
                // This lookup is covered by the unique key index and never
                // reads the data column (or its overflow pages).
                let rowid: Option<i64> = conn
                    .query_row("SELECT rowid FROM blobs WHERE blob_key = ?1", [&key], |r| {
                        r.get(0)
                    })
                    .optional()
                    .map_err(|e| e.to_string())?;
                let Some(rowid) = rowid else {
                    return Ok(None);
                };
                let blob = conn
                    .blob_open(rusqlite::DatabaseName::Main, "blobs", "data", rowid, true)
                    .map_err(|e| e.to_string())?;
                let length = blob.len() as u64;
                let size = length.saturating_sub(offset).min(max_bytes as u64) as usize;
                let mut bytes = vec![0; size];
                if size > 0 {
                    blob.read_at_exact(&mut bytes, offset as usize)
                        .map_err(|e| e.to_string())?;
                }
                Ok(Some((length, bytes)))
            })
            .await
            .map_err(|e| e.to_string())?;
        }
        // A remote database evaluates the slice outside the server process.
        // An in-memory fixture already holds the source in memory.
        let conn = self
            .configured_connection()
            .await
            .map_err(|e| e.to_string())?;
        let mut rows = conn
            .query(
                "SELECT length(data), substr(data, ?2, ?3) FROM blobs WHERE blob_key = ?1",
                params![key, start, count],
            )
            .await
            .map_err(|e| e.to_string())?;
        let Some(row) = rows.next().await.map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let length: i64 = row.get(0).map_err(|e| e.to_string())?;
        let bytes = match row.get_value(1).map_err(|e| e.to_string())? {
            crate::driver::Value::Blob(bytes) => bytes,
            _ => return Err("blob column is not BLOB type".to_string()),
        };
        Ok(Some((length as u64, bytes)))
    }
}
