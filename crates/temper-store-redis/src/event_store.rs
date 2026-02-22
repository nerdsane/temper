//! Redis-backed implementation of the [`EventStore`] trait.
//!
//! Uses Redis primitives:
//! - `LIST` per entity for ordered event journal entries
//! - `STRING` per entity for latest sequence number
//! - `STRING` per entity for snapshots
//! - `SET` per tenant to track distinct `(entity_type, entity_id)` pairs
//!
//! The `append()` operation uses a Lua script (`EVALSHA`) to atomically
//! check-and-set the sequence number, preventing lost-update races between
//! concurrent writers on the same entity.

use std::sync::Arc;

use fred::prelude::*;
use fred::types::scripts::Script;
use serde::{Deserialize, Serialize};
use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};

/// Lua script for atomic append: check expected sequence, RPUSH events, SET new seq, SADD entity ref.
///
/// KEYS[1] = seq_key, KEYS[2] = events_key, KEYS[3] = entities_key
/// ARGV[1] = expected_seq (string-encoded integer)
/// ARGV[2] = entity_ref_json (for SADD into entities set)
/// ARGV[3..N] = serialized event JSONs
///
/// Returns: `{1, new_seq}` on success, `{0, current_seq}` on conflict.
const APPEND_LUA: &str = r#"
local seq_key = KEYS[1]
local events_key = KEYS[2]
local entities_key = KEYS[3]
local expected = tonumber(ARGV[1])
local entity_ref = ARGV[2]

local current = tonumber(redis.call('GET', seq_key) or '0')
if current ~= expected then
    return {0, current}
end

for i = 3, #ARGV do
    redis.call('RPUSH', events_key, ARGV[i])
end

local new_seq = expected + (#ARGV - 2)
redis.call('SET', seq_key, tostring(new_seq))
redis.call('SADD', entities_key, entity_ref)

return {1, new_seq}
"#;

/// Redis-backed event store.
#[derive(Clone)]
pub struct RedisEventStore {
    client: Arc<fred::clients::Client>,
    append_script: Script,
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
            append_script: Script::from_lua(APPEND_LUA),
        })
    }

    /// Return a reference to the underlying Redis client.
    pub fn client(&self) -> &fred::clients::Client {
        &self.client
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

    fn trajectory_key(tenant: &str) -> String {
        format!("{}:trajectories:{tenant}", crate::keys::PREFIX)
    }

    /// Persist a trajectory entry as JSON into a capped Redis list.
    ///
    /// Uses RPUSH + LTRIM to maintain a bounded list of recent entries.
    pub async fn persist_trajectory(
        &self,
        tenant: &str,
        entry_json: &str,
        max_entries: i64,
    ) -> Result<(), PersistenceError> {
        let key = Self::trajectory_key(tenant);
        let _: i64 = self
            .client
            .rpush(&key, entry_json.to_string())
            .await
            .map_err(storage_error)?;
        // Trim to keep only the last `max_entries` items.
        let _: () = self
            .client
            .ltrim(&key, -max_entries, -1)
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    /// Load recent trajectory entries from Redis (newest last).
    pub async fn load_recent_trajectories(
        &self,
        tenant: &str,
        limit: i64,
    ) -> Result<Vec<String>, PersistenceError> {
        let key = Self::trajectory_key(tenant);
        let entries: Vec<String> = self
            .client
            .lrange(&key, -limit, -1)
            .await
            .map_err(storage_error)?;
        Ok(entries)
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
        let seq_key = Self::seq_key(tenant, entity_type, entity_id);
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let entities_key = Self::tenant_entities_key(tenant);

        // Pre-serialize events with provisional sequence numbers.
        let mut args: Vec<String> = Vec::with_capacity(events.len() + 2);
        args.push(expected_sequence.to_string());

        let entity_ref = EntityRef {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        };
        let entity_ref_json = serde_json::to_string(&entity_ref)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        args.push(entity_ref_json);

        let mut seq = expected_sequence;
        for event in events {
            seq += 1;
            let mut env = event.clone();
            env.sequence_nr = seq;
            let encoded = serde_json::to_string(&env)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            args.push(encoded);
        }

        let keys = vec![seq_key, events_key, entities_key];
        let result: Vec<i64> = self
            .append_script
            .evalsha_with_reload(&self.client, keys, args)
            .await
            .map_err(storage_error)?;

        match result.as_slice() {
            [1, new_seq] => Ok(*new_seq as u64),
            [0, actual] => Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: *actual as u64,
            }),
            other => Err(PersistenceError::Storage(format!(
                "unexpected Lua script result: {other:?}"
            ))),
        }
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let events_key = Self::events_key(tenant, entity_type, entity_id);

        // Events are stored via RPUSH with sequential indices starting at 0.
        // Event at index i has sequence_nr = i + 1.
        // To read events with sequence_nr > from_sequence, start at index from_sequence.
        let start_index = from_sequence as i64;
        let encoded_events: Vec<String> = self
            .client
            .lrange(&events_key, start_index, -1)
            .await
            .map_err(storage_error)?;

        let mut out = Vec::with_capacity(encoded_events.len());
        for encoded in encoded_events {
            let env: PersistenceEnvelope = serde_json::from_str(&encoded)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            out.push(env);
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

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::persistence::EventMetadata;

    fn redis_url() -> Option<String> {
        std::env::var("REDIS_URL").ok()
    }

    fn test_envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload,
            metadata: EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
                actor_id: "redis-test".to_string(),
            },
        }
    }

    fn unique_persistence_id() -> String {
        let id = uuid::Uuid::new_v4();
        format!("test-{id}:Order:ord-{id}")
    }

    async fn make_store() -> Option<RedisEventStore> {
        let url = redis_url()?;
        Some(
            RedisEventStore::new(&url)
                .await
                .expect("failed to connect to Redis"),
        )
    }

    #[tokio::test]
    async fn append_and_read_events_roundtrip() {
        let Some(store) = make_store().await else {
            eprintln!("REDIS_URL not set, skipping test");
            return;
        };
        let pid = unique_persistence_id();

        let new_seq = store
            .append(
                &pid,
                0,
                &[
                    test_envelope("OrderCreated", serde_json::json!({ "id": "ord-1" })),
                    test_envelope("OrderApproved", serde_json::json!({ "approved": true })),
                ],
            )
            .await
            .unwrap();

        assert_eq!(new_seq, 2);

        // Read all events
        let events = store.read_events(&pid, 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence_nr, 1);
        assert_eq!(events[1].sequence_nr, 2);
        assert_eq!(events[0].event_type, "OrderCreated");
        assert_eq!(events[1].event_type, "OrderApproved");

        // Partial read (from_sequence = 1 should skip event 1)
        let partial = store.read_events(&pid, 1).await.unwrap();
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].sequence_nr, 2);
        assert_eq!(partial[0].event_type, "OrderApproved");
    }

    #[tokio::test]
    async fn append_with_wrong_sequence_fails() {
        let Some(store) = make_store().await else {
            eprintln!("REDIS_URL not set, skipping test");
            return;
        };
        let pid = unique_persistence_id();

        store
            .append(
                &pid,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-1" }),
                )],
            )
            .await
            .unwrap();

        let err = store
            .append(
                &pid,
                0, // stale: actual is 1
                &[test_envelope(
                    "OrderUpdated",
                    serde_json::json!({ "step": 2 }),
                )],
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            PersistenceError::ConcurrencyViolation {
                expected: 0,
                actual: 1
            }
        ));
    }

    #[tokio::test]
    async fn snapshot_save_and_load_roundtrip() {
        let Some(store) = make_store().await else {
            eprintln!("REDIS_URL not set, skipping test");
            return;
        };
        let pid = unique_persistence_id();

        store
            .save_snapshot(&pid, 5, b"{\"status\":\"created\"}")
            .await
            .unwrap();

        let snapshot = store.load_snapshot(&pid).await.unwrap();
        assert_eq!(snapshot, Some((5, b"{\"status\":\"created\"}".to_vec())));

        // Overwrite
        store
            .save_snapshot(&pid, 8, b"{\"status\":\"shipped\"}")
            .await
            .unwrap();

        let updated = store.load_snapshot(&pid).await.unwrap();
        assert_eq!(updated, Some((8, b"{\"status\":\"shipped\"}".to_vec())));
    }

    #[tokio::test]
    async fn list_entity_ids_returns_distinct_pairs() {
        let Some(store) = make_store().await else {
            eprintln!("REDIS_URL not set, skipping test");
            return;
        };
        let unique = uuid::Uuid::new_v4();
        let tenant_a = format!("tenant-a-{unique}");
        let tenant_b = format!("tenant-b-{unique}");

        let order_1 = format!("{tenant_a}:Order:ord-1");
        let order_2 = format!("{tenant_a}:Order:ord-2");
        let task_1 = format!("{tenant_a}:Task:task-1");
        let other_tenant = format!("{tenant_b}:Order:ord-9");

        store
            .append(
                &order_1,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-1" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &order_2,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-2" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &task_1,
                0,
                &[test_envelope(
                    "TaskCreated",
                    serde_json::json!({ "id": "task-1" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &other_tenant,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-9" }),
                )],
            )
            .await
            .unwrap();

        let mut entities = store.list_entity_ids(&tenant_a).await.unwrap();
        entities.sort();

        assert_eq!(
            entities,
            vec![
                ("Order".to_string(), "ord-1".to_string()),
                ("Order".to_string(), "ord-2".to_string()),
                ("Task".to_string(), "task-1".to_string()),
            ]
        );

        // Cross-tenant isolation
        let other_entities = store.list_entity_ids(&tenant_b).await.unwrap();
        assert_eq!(
            other_entities,
            vec![("Order".to_string(), "ord-9".to_string())]
        );
    }

    #[tokio::test]
    async fn concurrent_appends_detect_conflict() {
        let Some(store) = make_store().await else {
            eprintln!("REDIS_URL not set, skipping test");
            return;
        };
        let pid = unique_persistence_id();

        let store1 = store.clone();
        let store2 = store.clone();
        let pid1 = pid.clone();
        let pid2 = pid.clone();

        let handle1 = tokio::spawn(async move {
            store1
                .append(
                    &pid1,
                    0,
                    &[test_envelope(
                        "OrderCreated",
                        serde_json::json!({ "writer": 1 }),
                    )],
                )
                .await
        });

        let handle2 = tokio::spawn(async move {
            store2
                .append(
                    &pid2,
                    0,
                    &[test_envelope(
                        "OrderCreated",
                        serde_json::json!({ "writer": 2 }),
                    )],
                )
                .await
        });

        let (r1, r2) = tokio::join!(handle1, handle2);
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();

        // Exactly one should succeed, the other should get a ConcurrencyViolation.
        let successes = [r1.is_ok(), r2.is_ok()].iter().filter(|&&ok| ok).count();
        let conflicts = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Err(PersistenceError::ConcurrencyViolation { .. })))
            .count();

        assert_eq!(successes, 1, "exactly one writer should succeed");
        assert_eq!(conflicts, 1, "exactly one writer should see a conflict");
    }
}
