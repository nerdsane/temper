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
use temper_runtime::persistence::schema_deployment::{
    SchemaScope, scoped_journal_pin_prefix, split_scoped_journal_entity_id,
};
use temper_runtime::persistence::{
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
    storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::keys::{decode_lex_component, encode_lex_component};

/// Lua script for atomic append: check expected sequence, append events, and index the journal.
///
/// KEYS[1] = seq_key, KEYS[2] = events_key, KEYS[3] = entities_key,
/// KEYS[4] = journals_key
/// ARGV[1] = expected_seq (string-encoded integer)
/// ARGV[2] = entity_ref_json (for SADD into entities set)
/// ARGV[3] = journal_member (order-preserving encoded type and id)
/// ARGV[4..N] = serialized event JSONs
///
/// Returns: `{1, new_seq}` on success, `{0, current_seq}` on conflict.
const APPEND_LUA: &str = r#"
local seq_key = KEYS[1]
local events_key = KEYS[2]
local entities_key = KEYS[3]
local journals_key = KEYS[4]
local expected = tonumber(ARGV[1])
local entity_ref = ARGV[2]
local journal_member = ARGV[3]

local current = tonumber(redis.call('GET', seq_key) or '0')
if current ~= expected then
    return {0, current}
end

for i = 4, #ARGV do
    redis.call('RPUSH', events_key, ARGV[i])
end

local new_seq = expected + (#ARGV - 3)
redis.call('SET', seq_key, tostring(new_seq))
redis.call('SADD', entities_key, entity_ref)
redis.call('ZADD', journals_key, 0, journal_member)

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
struct SnapshotHistoryRecord {
    sequence_nr: u64,
    snapshot: Vec<u8>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SegmentRecord {
    segment_index: u64,
    start_sequence_nr: u64,
    end_sequence_nr: Option<u64>,
    snapshot_sequence: Option<u64>,
    event_count: u64,
    sealed_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
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

    fn snapshot_history_key(
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        sequence_nr: u64,
    ) -> String {
        format!(
            "{}:snapshot_history:{tenant}:{entity_type}:{entity_id}:{sequence_nr}",
            crate::keys::PREFIX
        )
    }

    fn current_segment_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
        format!(
            "{}:event_segment_current:{tenant}:{entity_type}:{entity_id}",
            crate::keys::PREFIX
        )
    }

    fn segment_key(tenant: &str, entity_type: &str, entity_id: &str, segment_index: u64) -> String {
        format!(
            "{}:event_segment:{tenant}:{entity_type}:{entity_id}:{segment_index}",
            crate::keys::PREFIX
        )
    }

    fn tenant_entities_key(tenant: &str) -> String {
        format!("{}:entities:{tenant}", crate::keys::PREFIX)
    }

    fn tenant_journals_key(tenant: &str) -> String {
        format!("{}:journals:{tenant}", crate::keys::PREFIX)
    }

    fn journal_member(entity_type: &str, entity_id: &str) -> String {
        format!(
            "{}!{}",
            encode_lex_component(entity_type),
            encode_lex_component(entity_id)
        )
    }

    fn parse_journal_member(member: &str) -> Result<(String, String), PersistenceError> {
        let (entity_type, entity_id) = member.split_once('!').ok_or_else(|| {
            PersistenceError::Serialization("invalid Redis journal index member".to_string())
        })?;
        Ok((
            decode_lex_component(entity_type)?,
            decode_lex_component(entity_id)?,
        ))
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

impl EventStore for RedisEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let seq_key = Self::seq_key(tenant, entity_type, entity_id);
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let entities_key = Self::tenant_entities_key(tenant);
        let journals_key = Self::tenant_journals_key(tenant);

        // Pre-serialize events with provisional sequence numbers.
        let mut args: Vec<String> = Vec::with_capacity(events.len() + 3);
        args.push(expected_sequence.to_string());

        let entity_ref = EntityRef {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        };
        let entity_ref_json = serde_json::to_string(&entity_ref)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        args.push(entity_ref_json);
        args.push(Self::journal_member(entity_type, entity_id));

        let mut seq = expected_sequence;
        for event in events {
            seq += 1;
            let mut env = event.clone();
            env.sequence_nr = seq;
            let encoded = serde_json::to_string(&env)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            args.push(encoded);
        }

        let keys = vec![seq_key, events_key, entities_key, journals_key];
        let result: Vec<i64> = self
            .append_script
            .evalsha_with_reload(&self.client, keys, args)
            .await
            .map_err(storage_error)?;

        match result.as_slice() {
            [1, new_seq] => {
                let new_seq = *new_seq as u64;
                let current_segment_key = Self::current_segment_key(tenant, entity_type, entity_id);
                let current_segment_raw: Option<String> = self
                    .client
                    .get(&current_segment_key)
                    .await
                    .map_err(storage_error)?;
                let segment_index = current_segment_raw
                    .and_then(|raw| raw.parse::<u64>().ok())
                    .unwrap_or(0);
                let segment_key = Self::segment_key(tenant, entity_type, entity_id, segment_index);
                let existing: Option<String> =
                    self.client.get(&segment_key).await.map_err(storage_error)?;
                let mut record = existing
                    .as_deref()
                    .map(serde_json::from_str::<SegmentRecord>)
                    .transpose()
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?
                    .unwrap_or_else(|| SegmentRecord {
                        segment_index,
                        start_sequence_nr: (expected_sequence + 1).max(1),
                        end_sequence_nr: None,
                        snapshot_sequence: None,
                        event_count: 0,
                        sealed_at: None,
                        created_at: chrono::Utc::now(),
                    });
                record.end_sequence_nr = Some(new_seq);
                record.event_count = new_seq.saturating_sub(record.start_sequence_nr) + 1;
                let encoded = serde_json::to_string(&record)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
                let _: () = self
                    .client
                    .set(&segment_key, encoded, None, None, false)
                    .await
                    .map_err(storage_error)?;
                let _: () = self
                    .client
                    .set(
                        &current_segment_key,
                        segment_index.to_string(),
                        None,
                        None,
                        false,
                    )
                    .await
                    .map_err(storage_error)?;
                Ok(new_seq)
            }
            [0, actual] => Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: *actual as u64,
            }),
            other => Err(PersistenceError::Storage(format!(
                "unexpected Lua script result: {other:?}"
            ))),
        }
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        match appends {
            [] => Ok(Vec::new()),
            [append] => {
                let sequence_nr = self
                    .append(
                        &append.persistence_id,
                        append.expected_sequence,
                        &append.events,
                    )
                    .await?;
                Ok(vec![PersistenceAppendResult {
                    persistence_id: append.persistence_id.clone(),
                    sequence_nr,
                }])
            }
            _ => Err(PersistenceError::Storage(
                "RedisEventStore does not support atomic multi-journal append_batch".to_string(),
            )),
        }
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
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

    async fn read_events_limited(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let end = from_sequence
            .saturating_add(limit as u64)
            .saturating_sub(1)
            .min(i64::MAX as u64) as i64;
        let encoded_events: Vec<String> = self
            .client
            .lrange(&events_key, from_sequence.min(i64::MAX as u64) as i64, end)
            .await
            .map_err(storage_error)?;
        encoded_events
            .into_iter()
            .map(|encoded| {
                serde_json::from_str(&encoded)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))
            })
            .collect()
    }

    async fn read_latest_events(
        &self,
        persistence_id: &str,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let start = -(limit.min(i64::MAX as usize) as i64);
        let encoded_events: Vec<String> = self
            .client
            .lrange(&events_key, start, -1)
            .await
            .map_err(storage_error)?;
        encoded_events
            .into_iter()
            .map(|encoded| {
                serde_json::from_str(&encoded)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))
            })
            .collect()
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
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

        let history_key = Self::snapshot_history_key(tenant, entity_type, entity_id, sequence_nr);
        let history = SnapshotHistoryRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
            created_at: chrono::Utc::now(),
        };
        let encoded_history = serde_json::to_string(&history)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let _: () = self
            .client
            .set(&history_key, encoded_history, None, None, false)
            .await
            .map_err(storage_error)?;

        let current_segment_key = Self::current_segment_key(tenant, entity_type, entity_id);
        let current_segment_raw: Option<String> = self
            .client
            .get(&current_segment_key)
            .await
            .map_err(storage_error)?;
        let current_segment = current_segment_raw
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);
        let segment_key = Self::segment_key(tenant, entity_type, entity_id, current_segment);
        let existing: Option<String> =
            self.client.get(&segment_key).await.map_err(storage_error)?;
        let mut segment = existing
            .as_deref()
            .map(serde_json::from_str::<SegmentRecord>)
            .transpose()
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?
            .unwrap_or_else(|| SegmentRecord {
                segment_index: current_segment,
                start_sequence_nr: 1,
                end_sequence_nr: Some(sequence_nr),
                snapshot_sequence: Some(sequence_nr),
                event_count: sequence_nr,
                sealed_at: None,
                created_at: chrono::Utc::now(),
            });
        segment.end_sequence_nr = Some(sequence_nr);
        segment.snapshot_sequence = Some(sequence_nr);
        segment.event_count = sequence_nr.saturating_sub(segment.start_sequence_nr) + 1;
        segment.sealed_at = Some(chrono::Utc::now());
        let encoded_segment = serde_json::to_string(&segment)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let _: () = self
            .client
            .set(&segment_key, encoded_segment, None, None, false)
            .await
            .map_err(storage_error)?;

        let next_segment = current_segment + 1;
        let next_segment_key = Self::segment_key(tenant, entity_type, entity_id, next_segment);
        let next = SegmentRecord {
            segment_index: next_segment,
            start_sequence_nr: sequence_nr + 1,
            end_sequence_nr: None,
            snapshot_sequence: None,
            event_count: 0,
            sealed_at: None,
            created_at: chrono::Utc::now(),
        };
        let encoded_next = serde_json::to_string(&next)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let _: () = self
            .client
            .set(&next_segment_key, encoded_next, None, None, false)
            .await
            .map_err(storage_error)?;
        let _: () = self
            .client
            .set(
                &current_segment_key,
                next_segment.to_string(),
                None,
                None,
                false,
            )
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
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

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let key = Self::tenant_entities_key(tenant);
        let members: Vec<String> = self.client.smembers(&key).await.map_err(storage_error)?;

        let mut out = Vec::new();
        for encoded in members {
            let entity_ref: EntityRef = serde_json::from_str(&encoded)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            if entity_ref.entity_type == entity_type {
                out.push(entity_ref.entity_id);
            }
        }

        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn list_journal_ids_page(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let key = Self::tenant_journals_key(tenant);
        let (min, max) = match entity_type {
            Some(wanted) => {
                if after.is_some_and(|(after_type, _)| after_type > wanted) {
                    return Ok(Vec::new());
                }
                let prefix = format!("{}!", encode_lex_component(wanted));
                let min = match after {
                    Some((after_type, after_id)) if after_type == wanted => {
                        format!("({}", Self::journal_member(after_type, after_id))
                    }
                    _ => format!("[{prefix}"),
                };
                (min, format!("[{prefix}~"))
            }
            None => (
                after.map_or_else(
                    || "-".to_string(),
                    |(after_type, after_id)| {
                        format!("({}", Self::journal_member(after_type, after_id))
                    },
                ),
                "+".to_string(),
            ),
        };
        let count = limit.min(i64::MAX as usize) as i64;
        let members: Vec<String> = self
            .client
            .zrangebylex(&key, min, max, Some((0, count)))
            .await
            .map_err(storage_error)?;
        members
            .into_iter()
            .map(|member| Self::parse_journal_member(&member))
            .collect()
    }

    async fn scoped_entity_bundle_digests(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        scope: &SchemaScope,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let key = Self::tenant_journals_key(tenant);
        let type_prefix = encode_lex_component(entity_type);
        let entity_prefix = encode_lex_component(&scoped_journal_pin_prefix(entity_id, scope));
        let member_prefix = format!("{type_prefix}!{entity_prefix}");
        const PIN_SCAN_BUDGET: usize = 256;
        let count = PIN_SCAN_BUDGET.min(i64::MAX as usize) as i64;
        let members: Vec<String> = self
            .client
            .zrangebylex(
                &key,
                format!("[{member_prefix}"),
                format!("[{member_prefix}~"),
                Some((0, count)),
            )
            .await
            .map_err(storage_error)?;
        let scan_budget_exhausted = members.len() == PIN_SCAN_BUDGET;
        let mut digests = Vec::new();
        for member in members {
            let (_, scoped_id) = Self::parse_journal_member(&member)?;
            if let Some((found_entity_id, pin)) = split_scoped_journal_entity_id(&scoped_id)
                && found_entity_id == entity_id
                && &pin.scope == scope
            {
                digests.push(pin.bundle_digest);
                if digests.len() == limit {
                    break;
                }
            }
        }
        if scan_budget_exhausted && digests.len() < limit {
            return Err(PersistenceError::Storage(
                "scoped entity pin scan budget exhausted".to_string(),
            ));
        }
        Ok(digests)
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

    #[path = "scoped_schema_pin_test.rs"]
    mod scoped_schema_pin;

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
