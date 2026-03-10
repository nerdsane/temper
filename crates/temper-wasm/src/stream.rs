//! Dumb byte store for passing binary data between host and WASM guest.
//!
//! Two categories of entries:
//! - **Streams**: temporary, per-invocation (registered before WASM, consumed after)
//! - **Cache**: persisted across invocations, LRU safety eviction to prevent OOM
//!
//! WASM modules control caching strategy via `host_cache_*` functions.
//! StreamRegistry itself makes NO caching decisions -- it just stores bytes.
//!
//! Uses `BTreeMap` + `VecDeque` for deterministic LRU (DST compliance).

use std::collections::{BTreeMap, VecDeque};

/// Configuration for StreamRegistry safety limits.
#[derive(Debug, Clone)]
pub struct StreamRegistryConfig {
    /// Maximum cache size in bytes (safety budget, not policy).
    pub max_cache_bytes: usize,
}

impl Default for StreamRegistryConfig {
    fn default() -> Self {
        Self {
            max_cache_bytes: 256 * 1024 * 1024, // 256 MB
        }
    }
}

/// Dumb byte store for passing binary data between host and WASM guest.
///
/// Streams are temporary (per-invocation lifecycle). Cache entries persist
/// across invocations with LRU safety eviction to prevent OOM.
pub struct StreamRegistry {
    /// Active streams (temporary, per-invocation).
    streams: BTreeMap<String, Vec<u8>>,
    /// Cache entries: key -> bytes (persisted, managed by WASM via host_cache_*).
    cache: BTreeMap<String, Vec<u8>>,
    /// LRU order for safety eviction (most recent at back).
    lru_order: VecDeque<String>,
    /// Total cached bytes.
    cached_bytes: usize,
    /// Max cache size in bytes (safety budget, not policy).
    max_cache_bytes: usize,
}

impl StreamRegistry {
    /// Create a new StreamRegistry with default config.
    pub fn new() -> Self {
        Self::with_config(StreamRegistryConfig::default())
    }

    /// Create a new StreamRegistry with custom config.
    pub fn with_config(config: StreamRegistryConfig) -> Self {
        Self {
            streams: BTreeMap::new(),
            cache: BTreeMap::new(),
            lru_order: VecDeque::new(),
            cached_bytes: 0,
            max_cache_bytes: config.max_cache_bytes,
        }
    }

    // --- Stream methods (framework-controlled, per-invocation lifecycle) ---

    /// Register a temporary stream for WASM to reference by ID.
    pub fn register_stream(&mut self, id: &str, bytes: Vec<u8>) {
        self.streams.insert(id.to_string(), bytes);
    }

    /// Consume and remove a temporary stream.
    pub fn take_stream(&mut self, id: &str) -> Option<Vec<u8>> {
        self.streams.remove(id)
    }

    /// Store bytes into a stream (e.g., from HTTP response body).
    pub fn store_stream(&mut self, id: &str, bytes: Vec<u8>) {
        self.streams.insert(id.to_string(), bytes);
    }

    /// Read stream bytes without consuming (for hashing, uploading).
    pub fn get_stream(&self, id: &str) -> Option<&[u8]> {
        self.streams.get(id).map(|v| v.as_slice())
    }

    /// Number of active streams.
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    // --- Cache methods (WASM-controlled via host functions) ---

    /// Store bytes in cache. Evicts LRU entries if over budget.
    pub fn cache_put(&mut self, key: &str, bytes: Vec<u8>) {
        let new_size = bytes.len();

        // Remove old entry if exists (to update LRU position)
        if let Some(old) = self.cache.remove(key) {
            self.cached_bytes -= old.len();
            self.lru_order.retain(|k| k != key);
        }

        // Evict LRU entries until we have room
        while self.cached_bytes + new_size > self.max_cache_bytes && !self.lru_order.is_empty() {
            if let Some(evict_key) = self.lru_order.pop_front() {
                if let Some(evicted) = self.cache.remove(&evict_key) {
                    self.cached_bytes -= evicted.len();
                }
            }
        }

        // Insert new entry
        self.cached_bytes += new_size;
        self.cache.insert(key.to_string(), bytes);
        self.lru_order.push_back(key.to_string());
    }

    /// Read from cache (updates LRU position).
    pub fn cache_get(&mut self, key: &str) -> Option<&[u8]> {
        if self.cache.contains_key(key) {
            // Update LRU position
            self.lru_order.retain(|k| k != key);
            self.lru_order.push_back(key.to_string());
            self.cache.get(key).map(|v| v.as_slice())
        } else {
            None
        }
    }

    /// Check if key is in cache without reading or updating LRU.
    pub fn cache_contains(&self, key: &str) -> bool {
        self.cache.contains_key(key)
    }

    /// Copy cached bytes to a temporary stream (for WASM to reference).
    /// Returns byte count on success, None if not cached.
    pub fn cache_to_stream(&mut self, key: &str, stream_id: &str) -> Option<usize> {
        if let Some(bytes) = self.cache.get(key) {
            let len = bytes.len();
            let bytes_clone = bytes.clone();
            // Update LRU position
            self.lru_order.retain(|k| k != key);
            self.lru_order.push_back(key.to_string());
            self.streams.insert(stream_id.to_string(), bytes_clone);
            Some(len)
        } else {
            None
        }
    }

    /// Total cached bytes.
    pub fn cached_bytes(&self) -> usize {
        self.cached_bytes
    }

    /// Number of cache entries.
    pub fn cache_count(&self) -> usize {
        self.cache.len()
    }

    /// Clear all temporary streams (call between invocations if needed).
    pub fn clear_streams(&mut self) {
        self.streams.clear();
    }
}

impl Default for StreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_register_and_take() {
        let mut reg = StreamRegistry::new();
        reg.register_stream("upload-1", b"hello".to_vec());
        assert_eq!(reg.stream_count(), 1);

        let bytes = reg.take_stream("upload-1").unwrap();
        assert_eq!(bytes, b"hello");
        assert_eq!(reg.stream_count(), 0);

        // Take again returns None
        assert!(reg.take_stream("upload-1").is_none());
    }

    #[test]
    fn stream_get_without_consuming() {
        let mut reg = StreamRegistry::new();
        reg.register_stream("s-1", b"data".to_vec());

        assert_eq!(reg.get_stream("s-1"), Some(b"data".as_slice()));
        assert_eq!(reg.stream_count(), 1); // Still there
    }

    #[test]
    fn cache_put_and_get() {
        let mut reg = StreamRegistry::new();
        reg.cache_put("sha256:abc", b"content".to_vec());

        assert!(reg.cache_contains("sha256:abc"));
        assert_eq!(reg.cache_get("sha256:abc"), Some(b"content".as_slice()));
        assert_eq!(reg.cached_bytes(), 7);
        assert_eq!(reg.cache_count(), 1);
    }

    #[test]
    fn cache_lru_eviction() {
        let config = StreamRegistryConfig {
            max_cache_bytes: 20,
        };
        let mut reg = StreamRegistry::with_config(config);

        reg.cache_put("a", vec![0u8; 10]); // 10 bytes
        reg.cache_put("b", vec![1u8; 10]); // 20 bytes total
        assert_eq!(reg.cache_count(), 2);

        // Adding "c" should evict "a" (LRU)
        reg.cache_put("c", vec![2u8; 10]); // Would be 30, evict "a" -> 20
        assert!(!reg.cache_contains("a"));
        assert!(reg.cache_contains("b"));
        assert!(reg.cache_contains("c"));
        assert_eq!(reg.cached_bytes(), 20);
    }

    #[test]
    fn cache_lru_access_updates_position() {
        let config = StreamRegistryConfig {
            max_cache_bytes: 20,
        };
        let mut reg = StreamRegistry::with_config(config);

        reg.cache_put("a", vec![0u8; 10]);
        reg.cache_put("b", vec![1u8; 10]);

        // Access "a" to move it to back of LRU
        reg.cache_get("a");

        // Adding "c" should evict "b" (now LRU) instead of "a"
        reg.cache_put("c", vec![2u8; 10]);
        assert!(reg.cache_contains("a")); // "a" was accessed, not LRU
        assert!(!reg.cache_contains("b")); // "b" was LRU
        assert!(reg.cache_contains("c"));
    }

    #[test]
    fn cache_to_stream_copies_bytes() {
        let mut reg = StreamRegistry::new();
        reg.cache_put("sha256:abc", b"cached-data".to_vec());

        let len = reg.cache_to_stream("sha256:abc", "download-1").unwrap();
        assert_eq!(len, 11);

        // Stream has the bytes
        assert_eq!(
            reg.get_stream("download-1"),
            Some(b"cached-data".as_slice())
        );

        // Cache still has the bytes
        assert!(reg.cache_contains("sha256:abc"));
    }

    #[test]
    fn cache_to_stream_missing_key() {
        let mut reg = StreamRegistry::new();
        assert!(reg.cache_to_stream("missing", "s-1").is_none());
    }

    #[test]
    fn cache_put_replaces_existing() {
        let mut reg = StreamRegistry::new();
        reg.cache_put("key", b"old".to_vec());
        assert_eq!(reg.cached_bytes(), 3);

        reg.cache_put("key", b"new-data".to_vec());
        assert_eq!(reg.cached_bytes(), 8);
        assert_eq!(reg.cache_get("key"), Some(b"new-data".as_slice()));
        assert_eq!(reg.cache_count(), 1);
    }

    #[test]
    fn cache_contains_without_lru_update() {
        let config = StreamRegistryConfig {
            max_cache_bytes: 20,
        };
        let mut reg = StreamRegistry::with_config(config);

        reg.cache_put("a", vec![0u8; 10]);
        reg.cache_put("b", vec![1u8; 10]);

        // contains does NOT update LRU
        assert!(reg.cache_contains("a"));

        // "a" should still be LRU
        reg.cache_put("c", vec![2u8; 10]);
        assert!(!reg.cache_contains("a")); // "a" evicted
        assert!(reg.cache_contains("b"));
        assert!(reg.cache_contains("c"));
    }

    #[test]
    fn clear_streams_preserves_cache() {
        let mut reg = StreamRegistry::new();
        reg.register_stream("s-1", b"temp".to_vec());
        reg.cache_put("c-1", b"cached".to_vec());

        reg.clear_streams();

        assert_eq!(reg.stream_count(), 0);
        assert!(reg.cache_contains("c-1"));
    }

    #[test]
    fn deterministic_eviction_order() {
        // Verify BTreeMap + VecDeque gives deterministic results
        let config = StreamRegistryConfig {
            max_cache_bytes: 30,
        };
        let mut reg = StreamRegistry::with_config(config);

        reg.cache_put("c", vec![0u8; 10]);
        reg.cache_put("a", vec![1u8; 10]);
        reg.cache_put("b", vec![2u8; 10]);

        // All three fit (30 bytes)
        assert_eq!(reg.cache_count(), 3);

        // Adding "d" should evict "c" (first inserted = LRU front)
        reg.cache_put("d", vec![3u8; 10]);
        assert!(!reg.cache_contains("c")); // "c" was first in, first out
        assert!(reg.cache_contains("a"));
        assert!(reg.cache_contains("b"));
        assert!(reg.cache_contains("d"));
    }
}
