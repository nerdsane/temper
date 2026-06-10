//! Content-addressed blob storage with optional TTL expiry.

use std::time::Duration;

use crate::PostgresEventStore;

impl PostgresEventStore {
    pub async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        self.put_blob_with_ttl(key, data, None).await
    }

    pub async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let ttl_seconds = ttl.map(|duration| duration.as_secs() as i64);
        crate::dbm::postgres_query!(
            "INSERT INTO blobs (blob_key, data, size_bytes, expires_at) \
             VALUES ($1, $2, $3, CASE WHEN $4::bigint IS NULL THEN NULL ELSE now() + ($4::bigint * interval '1 second') END) \
             ON CONFLICT (blob_key) DO NOTHING",
        )
        .bind(key)
        .bind(data)
        .bind(data.len() as i64)
        .bind(ttl_seconds)
        .execute(self.pool())
        .await
        .map(|_| ())
        .map_err(|e| format!("blob put failed: {e}"))
    }

    pub async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String> {
        let result = crate::dbm::postgres_query!(
            "WITH doomed AS ( \
                 SELECT blob_key FROM blobs \
                 WHERE expires_at IS NOT NULL AND expires_at < now() \
                 LIMIT $1 \
             ) \
             DELETE FROM blobs USING doomed WHERE blobs.blob_key = doomed.blob_key",
        )
        .bind(max_rows as i64)
        .execute(self.pool())
        .await
        .map_err(|e| format!("blob sweep failed: {e}"))?;
        Ok(result.rows_affected())
    }

    pub async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        crate::dbm::postgres_query_scalar!("SELECT data FROM blobs WHERE blob_key = $1")
            .bind(key)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| format!("blob get failed: {e}"))
    }
}
