//! Actor placement cache — tracks which node hosts which actor.
//!
//! Uses a HashMap in-memory (Redis HSET in production) for O(1) lookup.
//! The shard manager updates placement when actors migrate between nodes.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use fred::prelude::*;

/// Actor placement information.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Placement {
    /// The node hosting this actor.
    pub node_id: String,
    /// The shard this actor belongs to.
    pub shard_id: u32,
    /// When this placement was last updated.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Placement store trait for testability.
pub trait PlacementStore: Send + Sync + 'static {
    /// Look up where an entity actor is placed.
    fn get_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<Placement>, crate::error::RedisStoreError>> + Send;

    /// Set the placement for an entity actor.
    fn set_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
        placement: &Placement,
    ) -> impl std::future::Future<Output = Result<(), crate::error::RedisStoreError>> + Send;

    /// Remove placement (actor passivated).
    fn remove_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> impl std::future::Future<Output = Result<(), crate::error::RedisStoreError>> + Send;

    /// Get all placements for a given shard (for rebalancing).
    fn get_shard_placements(
        &self,
        shard_id: u32,
    ) -> impl std::future::Future<Output = Result<Vec<(String, Placement)>, crate::error::RedisStoreError>> + Send;
}

/// Redis-backed placement store.
///
/// Each entity actor's placement is stored as a JSON string at
/// `temper:placement:{entity_type}:{entity_id}`. Shard queries use SCAN
/// to iterate matching keys and filter by shard_id.
pub struct RedisPlacement {
    client: Arc<fred::clients::Client>,
}

impl RedisPlacement {
    /// Create a new Redis-backed placement store.
    pub fn new(client: Arc<fred::clients::Client>) -> Self {
        Self { client }
    }
}

impl PlacementStore for RedisPlacement {
    async fn get_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<Placement>, crate::error::RedisStoreError> {
        let key = crate::keys::placement_key(entity_type, entity_id);
        let value: Option<String> = self
            .client
            .get(&key)
            .await
            .map_err(|e| crate::error::RedisStoreError::Command(e.to_string()))?;

        match value {
            Some(json) => {
                let placement: Placement = serde_json::from_str(&json)
                    .map_err(|e| crate::error::RedisStoreError::Serialization(e.to_string()))?;
                Ok(Some(placement))
            }
            None => Ok(None),
        }
    }

    async fn set_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
        placement: &Placement,
    ) -> Result<(), crate::error::RedisStoreError> {
        let key = crate::keys::placement_key(entity_type, entity_id);
        let json = serde_json::to_string(placement)
            .map_err(|e| crate::error::RedisStoreError::Serialization(e.to_string()))?;

        let _: () = self
            .client
            .set(&key, json, None, None, false)
            .await
            .map_err(|e| crate::error::RedisStoreError::Command(e.to_string()))?;
        Ok(())
    }

    async fn remove_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), crate::error::RedisStoreError> {
        let key = crate::keys::placement_key(entity_type, entity_id);
        let _: i64 = self
            .client
            .del(&key)
            .await
            .map_err(|e| crate::error::RedisStoreError::Command(e.to_string()))?;
        Ok(())
    }

    async fn get_shard_placements(
        &self,
        shard_id: u32,
    ) -> Result<Vec<(String, Placement)>, crate::error::RedisStoreError> {
        // SCAN for all placement keys, GET each, and filter by shard_id.
        let pattern = format!("{}:placement:*", crate::keys::PREFIX);
        let mut scanner = std::pin::pin!(self.client.scan_buffered(&pattern, None, None));
        let mut result = Vec::new();

        loop {
            let item: Option<Result<fred::types::Key, fred::error::Error>> =
                std::future::poll_fn(|cx| {
                    futures_core::Stream::poll_next(scanner.as_mut(), cx)
                })
                .await;
            match item {
                Some(Ok(key)) => {
                    let value: Option<String> = self
                        .client
                        .get::<Option<String>, _>(&key)
                        .await
                        .map_err(|e: fred::error::Error| {
                            crate::error::RedisStoreError::Command(e.to_string())
                        })?;
                    if let Some(json) = value {
                        if let Ok(placement) = serde_json::from_str::<Placement>(&json) {
                            if placement.shard_id == shard_id {
                                let key_str = key.into_string().unwrap_or_default();
                                result.push((key_str, placement));
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    return Err(crate::error::RedisStoreError::Command(e.to_string()));
                }
                None => break,
            }
        }

        Ok(result)
    }
}

/// In-memory placement store for testing.
pub struct InMemoryPlacement {
    placements: Arc<RwLock<HashMap<String, Placement>>>,
}

impl InMemoryPlacement {
    pub fn new() -> Self {
        Self {
            placements: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn key(entity_type: &str, entity_id: &str) -> String {
        format!("{entity_type}:{entity_id}")
    }
}

impl Default for InMemoryPlacement {
    fn default() -> Self {
        Self::new()
    }
}

impl PlacementStore for InMemoryPlacement {
    async fn get_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<Placement>, crate::error::RedisStoreError> {
        let key = Self::key(entity_type, entity_id);
        Ok(self.placements.read().unwrap().get(&key).cloned())
    }

    async fn set_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
        placement: &Placement,
    ) -> Result<(), crate::error::RedisStoreError> {
        let key = Self::key(entity_type, entity_id);
        self.placements.write().unwrap().insert(key, placement.clone());
        Ok(())
    }

    async fn remove_placement(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), crate::error::RedisStoreError> {
        let key = Self::key(entity_type, entity_id);
        self.placements.write().unwrap().remove(&key);
        Ok(())
    }

    async fn get_shard_placements(
        &self,
        shard_id: u32,
    ) -> Result<Vec<(String, Placement)>, crate::error::RedisStoreError> {
        let placements = self.placements.read().unwrap();
        Ok(placements
            .iter()
            .filter(|(_, p)| p.shard_id == shard_id)
            .map(|(k, p)| (k.clone(), p.clone()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_placement(node: &str, shard: u32) -> Placement {
        Placement {
            node_id: node.to_string(),
            shard_id: shard,
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_set_and_get_placement() {
        let store = InMemoryPlacement::new();

        store.set_placement("Order", "abc", &test_placement("node-1", 3)).await.unwrap();
        let p = store.get_placement("Order", "abc").await.unwrap().unwrap();
        assert_eq!(p.node_id, "node-1");
        assert_eq!(p.shard_id, 3);
    }

    #[tokio::test]
    async fn test_get_missing_placement() {
        let store = InMemoryPlacement::new();
        let p = store.get_placement("Order", "missing").await.unwrap();
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn test_remove_placement() {
        let store = InMemoryPlacement::new();
        store.set_placement("Order", "abc", &test_placement("node-1", 1)).await.unwrap();
        store.remove_placement("Order", "abc").await.unwrap();
        assert!(store.get_placement("Order", "abc").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_shard_placements() {
        let store = InMemoryPlacement::new();
        store.set_placement("Order", "a", &test_placement("node-1", 5)).await.unwrap();
        store.set_placement("Order", "b", &test_placement("node-2", 5)).await.unwrap();
        store.set_placement("Order", "c", &test_placement("node-1", 7)).await.unwrap();

        let shard_5 = store.get_shard_placements(5).await.unwrap();
        assert_eq!(shard_5.len(), 2);

        let shard_7 = store.get_shard_placements(7).await.unwrap();
        assert_eq!(shard_7.len(), 1);
    }
}
