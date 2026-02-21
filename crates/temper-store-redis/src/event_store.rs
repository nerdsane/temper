//! Redis-backed implementation of the [`EventStore`] trait.
//!
//! Uses Redis primitives:
//! - `LIST` per entity for ordered event journal entries
//! - `STRING` per entity for latest sequence number
//! - `STRING` per entity for snapshots
//! - `SET` per tenant to track distinct `(entity_type, entity_id)` pairs

use std::sync::Arc;

use fred::prelude::*;
use serde::{Deserialize, Serialize};
use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};

/// Redis-backed event store.
#[derive(Clone)]
pub struct RedisEventStore {
    client: Arc<fred::clients::Client>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotRecord {
    sequence_nr: u64,
    snapshot: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EntityRef {
    entity_type: String,
    entity_id: String,
}

impl RedisEventStore {
    /// Connect to Redis using a URL such as `redis://localhost:6379/0`.
    pub async fn new(redis_url: &str) -> Result<Self, PersistenceError> {
        let config = Config::from_url(redis_url).map_err(storage_error)?;
        let client = Builder::from_config(config)
            .build()
            .map_err(storage_error)?;
        client.init().await.map_err(storage_error)?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    fn events_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
        format!(
            "{}:events:{tenant}:{entity_type}:{entity_id}",
            crate::keys::PREFIX
        )
    }

    fn seq_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
        format!(
            "{}:events_seq:{tenant}:{entity_type}:{entity_id}",
            crate::keys::PREFIX
        )
    }

    fn snapshot_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
        format!(
            "{}:snapshot:{tenant}:{entity_type}:{entity_id}",
            crate::keys::PREFIX
        )
    }

    fn tenant_entities_key(tenant: &str) -> String {
        format!("{}:entities:{tenant}", crate::keys::PREFIX)
    }
}

fn parse_persistence_id(persistence_id: &str) -> Result<(&str, &str, &str), PersistenceError> {
    let parts: Vec<&str> = persistence_id.splitn(3, ':').collect();
    match parts.len() {
        3 => {
            let tenant = parts[0];
            let entity_type = parts[1];
            let entity_id = parts[2];
            if tenant.is_empty() || entity_type.is_empty() || entity_id.is_empty() {
                return Err(PersistenceError::Storage(format!(
                    "invalid persistence_id (empty segment): {persistence_id}"
                )));
            }
            Ok((tenant, entity_type, entity_id))
        }
        2 => {
            let entity_type = parts[0];
            let entity_id = parts[1];
            if entity_type.is_empty() || entity_id.is_empty() {
                return Err(PersistenceError::Storage(format!(
                    "invalid persistence_id (empty segment): {persistence_id}"
                )));
            }
            Ok(("default", entity_type, entity_id))
        }
        _ => Err(PersistenceError::Storage(format!(
            "invalid persistence_id (expected 'tenant:type:id' or 'type:id'): {persistence_id}"
        ))),
    }
}

fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}

impl EventStore for RedisEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let seq_key = Self::seq_key(tenant, entity_type, entity_id);
        let entities_key = Self::tenant_entities_key(tenant);

        let current_seq = self
            .client
            .get::<Option<u64>, _>(&seq_key)
            .await
            .map_err(storage_error)?
            .unwrap_or(0);
        if current_seq != expected_sequence {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq += 1;
            let mut env = event.clone();
            env.sequence_nr = new_seq;
            let encoded = serde_json::to_string(&env)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            let _: i64 = self
                .client
                .rpush(&events_key, encoded)
                .await
                .map_err(storage_error)?;
        }

        let _: () = self
            .client
            .set(&seq_key, new_seq as i64, None, None, false)
            .await
            .map_err(storage_error)?;

        let entity_ref = EntityRef {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        };
        let entity_ref_json = serde_json::to_string(&entity_ref)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let _: i64 = self
            .client
            .sadd(&entities_key, vec![entity_ref_json])
            .await
            .map_err(storage_error)?;

        Ok(new_seq)
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let events_key = Self::events_key(tenant, entity_type, entity_id);

        let encoded_events: Vec<String> = self
            .client
            .lrange(&events_key, 0, -1)
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        for encoded in encoded_events {
            let env: PersistenceEnvelope = serde_json::from_str(&encoded)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            if env.sequence_nr > from_sequence {
                out.push(env);
            }
        }
        out.sort_by_key(|e| e.sequence_nr);
        Ok(out)
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let key = Self::snapshot_key(tenant, entity_type, entity_id);
        let record = SnapshotRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
        };
        let encoded = serde_json::to_string(&record)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let _: () = self
            .client
            .set(&key, encoded, None, None, false)
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let key = Self::snapshot_key(tenant, entity_type, entity_id);
        let encoded: Option<String> = self.client.get(&key).await.map_err(storage_error)?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let record: SnapshotRecord = serde_json::from_str(&encoded)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        Ok(Some((record.sequence_nr, record.snapshot)))
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let key = Self::tenant_entities_key(tenant);
        let members: Vec<String> = self.client.smembers(&key).await.map_err(storage_error)?;

        let mut out = Vec::with_capacity(members.len());
        for encoded in members {
            let entity_ref: EntityRef = serde_json::from_str(&encoded)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            out.push((entity_ref.entity_type, entity_ref.entity_id));
        }

        out.sort();
        out.dedup();
        Ok(out)
    }
}
