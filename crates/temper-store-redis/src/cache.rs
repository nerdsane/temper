//! Entity and function response cache.
//!
//! Provides TTL-based caching for:
//! - OData Function responses (read-only, safe to cache)
//! - Entity state snapshots (read-through for frequently accessed entities)

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use fred::prelude::*;

/// A cached value with TTL.
#[derive(Debug, Clone)]
struct CacheEntry {
    value: String,
    expires_at: Instant,
}

/// Cache store trait for testability.
pub trait CacheStore: Send + Sync + 'static {
    /// Get a cached value by key. Returns None if expired or missing.
    fn get(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, crate::error::RedisStoreError>> + Send;

    /// Set a cached value with TTL.
    fn set(
        &self,
        key: &str,
        value: &str,
        ttl: Duration,
    ) -> impl std::future::Future<Output = Result<(), crate::error::RedisStoreError>> + Send;

    /// Delete a cached value.
    fn delete(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = Result<(), crate::error::RedisStoreError>> + Send;

    /// Delete all cached values matching a prefix pattern.
    fn delete_prefix(
        &self,
        prefix: &str,
    ) -> impl std::future::Future<Output = Result<u64, crate::error::RedisStoreError>> + Send;
}

/// Redis-backed cache store using SET with EX for TTL.
///
/// Keys are stored as-is (caller provides the fully-qualified key).
/// TTL is enforced natively by Redis via the EX option.
pub struct RedisCache {
    client: Arc<fred::clients::Client>,
}

impl RedisCache {
    /// Create a new Redis-backed cache store.
    pub fn new(client: Arc<fred::clients::Client>) -> Self {
        Self { client }
    }
}

impl CacheStore for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<String>, crate::error::RedisStoreError> {
        let value: Option<String> = self
            .client
            .get(key)
            .await
            .map_err(|e| crate::error::RedisStoreError::Command(e.to_string()))?;
        Ok(value)
    }

    async fn set(
        &self,
        key: &str,
        value: &str,
        ttl: Duration,
    ) -> Result<(), crate::error::RedisStoreError> {
        let ttl_secs = ttl.as_secs().max(1); // Redis EX requires at least 1 second
        let expiration = Some(Expiration::EX(ttl_secs as i64));
        let _: () = self
            .client
            .set(key, value, expiration, None, false)
            .await
            .map_err(|e| crate::error::RedisStoreError::Command(e.to_string()))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), crate::error::RedisStoreError> {
        let _: i64 = self
            .client
            .del(key)
            .await
            .map_err(|e| crate::error::RedisStoreError::Command(e.to_string()))?;
        Ok(())
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<u64, crate::error::RedisStoreError> {
        let pattern = format!("{prefix}*");
        let mut scanner = std::pin::pin!(self.client.scan_buffered(&pattern, None, None));
        let mut total_deleted: u64 = 0;

        loop {
            let item: Option<Result<fred::types::Key, fred::error::Error>> =
                std::future::poll_fn(|cx| futures_core::Stream::poll_next(scanner.as_mut(), cx))
                    .await;
            match item {
                Some(Ok(key)) => {
                    let deleted: i64 =
                        self.client
                            .del::<i64, _>(key)
                            .await
                            .map_err(|e: fred::error::Error| {
                                crate::error::RedisStoreError::Command(e.to_string())
                            })?;
                    total_deleted += deleted as u64;
                }
                Some(Err(e)) => {
                    return Err(crate::error::RedisStoreError::Command(e.to_string()));
                }
                None => break,
            }
        }

        Ok(total_deleted)
    }
}

/// In-memory cache for testing (no Redis needed).
pub struct InMemoryCache {
    entries: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl InMemoryCache {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheStore for InMemoryCache {
    async fn get(&self, key: &str) -> Result<Option<String>, crate::error::RedisStoreError> {
        let entries = self.entries.read().unwrap();
        match entries.get(key) {
            Some(entry) if entry.expires_at > Instant::now() => Ok(Some(entry.value.clone())),
            Some(_) => Ok(None), // expired
            None => Ok(None),
        }
    }

    async fn set(
        &self,
        key: &str,
        value: &str,
        ttl: Duration,
    ) -> Result<(), crate::error::RedisStoreError> {
        let mut entries = self.entries.write().unwrap();
        entries.insert(
            key.to_string(),
            CacheEntry {
                value: value.to_string(),
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), crate::error::RedisStoreError> {
        self.entries.write().unwrap().remove(key);
        Ok(())
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<u64, crate::error::RedisStoreError> {
        let mut entries = self.entries.write().unwrap();
        let keys_to_remove: Vec<String> = entries
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        let count = keys_to_remove.len() as u64;
        for key in keys_to_remove {
            entries.remove(&key);
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_set_and_get() {
        let cache = InMemoryCache::new();
        cache
            .set("key1", "value1", Duration::from_secs(60))
            .await
            .unwrap();
        let v = cache.get("key1").await.unwrap();
        assert_eq!(v, Some("value1".to_string()));
    }

    #[tokio::test]
    async fn test_get_missing() {
        let cache = InMemoryCache::new();
        assert!(cache.get("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_ttl_expiry() {
        let cache = InMemoryCache::new();
        // Set with 0 TTL — already expired
        cache.set("key1", "value1", Duration::ZERO).await.unwrap();
        assert!(cache.get("key1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_delete() {
        let cache = InMemoryCache::new();
        cache
            .set("key1", "value1", Duration::from_secs(60))
            .await
            .unwrap();
        cache.delete("key1").await.unwrap();
        assert!(cache.get("key1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_delete_prefix() {
        let cache = InMemoryCache::new();
        cache
            .set("temper:cache:fn:A:1", "r1", Duration::from_secs(60))
            .await
            .unwrap();
        cache
            .set("temper:cache:fn:A:2", "r2", Duration::from_secs(60))
            .await
            .unwrap();
        cache
            .set("temper:cache:fn:B:1", "r3", Duration::from_secs(60))
            .await
            .unwrap();
        cache
            .set("temper:other:X", "r4", Duration::from_secs(60))
            .await
            .unwrap();

        let deleted = cache.delete_prefix("temper:cache:fn:A").await.unwrap();
        assert_eq!(deleted, 2);

        // B and other should still exist
        assert!(cache.get("temper:cache:fn:B:1").await.unwrap().is_some());
        assert!(cache.get("temper:other:X").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_overwrite() {
        let cache = InMemoryCache::new();
        cache.set("k", "v1", Duration::from_secs(60)).await.unwrap();
        cache.set("k", "v2", Duration::from_secs(60)).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap(), Some("v2".to_string()));
    }
}
