//! Redis-backed implementation of the [`EventStore`] trait.
//!
//! Uses Redis primitives:
//! - `LIST` per entity for ordered event journal entries
//! - `STRING` per entity for latest sequence number
//! - `STRING` per entity for snapshots
//! - `SET` plus a lexicographically ordered `ZSET` per tenant to discover streams
//!
//! The `append()` operation uses a Lua script (`EVALSHA`) to atomically
//! check-and-set the sequence number, preventing lost-update races between
//! concurrent writers on the same entity.

use std::sync::Arc;

use fred::prelude::*;
use fred::types::scripts::Script;
use serde::{Deserialize, Serialize};
use temper_runtime::persistence::{
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
    PersistenceSequenceGuard, storage_error, validate_guarded_persistence_append_batch,
    validate_latest_event_batch,
};
use temper_runtime::tenant::parse_persistence_id_parts;

mod ordered_index;

// Redis Lua 5.1 represents numbers as IEEE-754 doubles. Keeping journal and
// snapshot sequences inside the exact-integer range prevents silent rounding
// in optimistic-concurrency and monotonic-snapshot comparisons.
const MAX_SAFE_REDIS_SEQUENCE: u64 = 9_007_199_254_740_991;

fn redis_sequence(sequence_nr: u64, operation: &str) -> Result<i64, PersistenceError> {
    if sequence_nr > MAX_SAFE_REDIS_SEQUENCE {
        return Err(PersistenceError::Storage(format!(
            "event sequence exceeds Redis Lua exact-integer range during {operation}"
        )));
    }
    Ok(sequence_nr as i64)
}

fn decoded_redis_sequence(sequence_nr: i64, operation: &str) -> Result<u64, PersistenceError> {
    let sequence_nr = u64::try_from(sequence_nr).map_err(|_| {
        PersistenceError::Storage(format!(
            "Redis returned a negative event sequence during {operation}"
        ))
    })?;
    redis_sequence(sequence_nr, operation)?;
    Ok(sequence_nr)
}

/// Lua script for atomic append: check expected sequence, append events, and index the stream.
///
/// KEYS[1] = seq_key, KEYS[2] = events_key, KEYS[3] = entities_key,
/// KEYS[4] = ordered_entities_key, KEYS[5] = ordered_type_entities_key
/// ARGV[1] = expected_seq (string-encoded integer)
/// ARGV[2] = entity_ref_json (for SADD into entities set)
/// KEYS[6..N] = compare-only guard sequence keys
/// ARGV[3] = event count
/// ARGV[4..(3 + event_count)] = serialized event JSONs
/// remaining ARGV entries = expected guard sequences in KEYS order
///
/// Returns: `{1, new_seq}` on success, `{0, current_seq}` on target conflict,
/// or `{2, one_based_guard_index, current_seq}` on guard conflict.
const APPEND_LUA: &str = r#"
local seq_key = KEYS[1]
local events_key = KEYS[2]
local entities_key = KEYS[3]
local ordered_entities_key = KEYS[4]
local ordered_type_entities_key = KEYS[5]
local expected = tonumber(ARGV[1])
local entity_ref = ARGV[2]
local event_count = tonumber(ARGV[3])
local max_safe_sequence = 9007199254740991

if not expected or expected < 0 or expected > max_safe_sequence or expected % 1 ~= 0 then
    return redis.error_reply('invalid expected event sequence')
end
if not event_count or event_count < 1 or event_count % 1 ~= 0 then
    return redis.error_reply('invalid event count')
end

for key_index = 6, #KEYS do
    local guard_arg_index = 3 + event_count + (key_index - 5)
    local guard_expected = tonumber(ARGV[guard_arg_index])
    if not guard_expected or guard_expected < 0 or guard_expected > max_safe_sequence or guard_expected % 1 ~= 0 then
        return redis.error_reply('invalid guard event sequence')
    end
    local guard_actual = tonumber(redis.call('GET', KEYS[key_index]) or '0')
    if not guard_actual or guard_actual < 0 or guard_actual > max_safe_sequence or guard_actual % 1 ~= 0 then
        return redis.error_reply('invalid current guard event sequence')
    end
    if guard_actual ~= guard_expected then
        return {2, key_index - 5, guard_actual}
    end
end

local current = tonumber(redis.call('GET', seq_key) or '0')
if not current or current < 0 or current > max_safe_sequence or current % 1 ~= 0 then
    return redis.error_reply('invalid current event sequence')
end
if current ~= expected then
    return {0, current}
end

if expected + event_count > max_safe_sequence then
    return redis.error_reply('event sequence exceeds exact-integer range')
end

for i = 4, 3 + event_count do
    redis.call('RPUSH', events_key, ARGV[i])
end

local new_seq = expected + event_count
redis.call('SET', seq_key, tostring(new_seq))
redis.call('SADD', entities_key, entity_ref)
redis.call('ZADD', ordered_entities_key, 0, entity_ref)
redis.call('ZADD', ordered_type_entities_key, 0, entity_ref)

return {1, new_seq}
"#;

/// Atomically retain the highest recovery snapshot and record immutable history.
///
/// KEYS[1] = latest snapshot key, KEYS[2] = sequence-specific history key
/// ARGV[1] = sequence, ARGV[2] = encoded latest record,
/// ARGV[3] = encoded history record
const SAVE_SNAPSHOT_LUA: &str = r#"
local latest_key = KEYS[1]
local history_key = KEYS[2]
local sequence = tonumber(ARGV[1])
local current = redis.call('GET', latest_key)
local max_safe_sequence = 9007199254740991

if not sequence or sequence < 0 or sequence > max_safe_sequence or sequence % 1 ~= 0 then
    return redis.error_reply('invalid snapshot sequence')
end

if current then
    local decoded = cjson.decode(current)
    local current_sequence = tonumber(decoded.sequence_nr)
    if not current_sequence or current_sequence < 0 or current_sequence > max_safe_sequence or current_sequence % 1 ~= 0 then
        return redis.error_reply('invalid current snapshot sequence')
    end
    if sequence >= current_sequence then
        redis.call('SET', latest_key, ARGV[2])
    end
else
    redis.call('SET', latest_key, ARGV[2])
end

redis.call('SET', history_key, ARGV[3])
return 1
"#;

/// Redis-backed event store.
#[derive(Clone)]
pub struct RedisEventStore {
    client: Arc<fred::clients::Client>,
    append_script: Script,
    save_snapshot_script: Script,
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
            save_snapshot_script: Script::from_lua(SAVE_SNAPSHOT_LUA),
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

    fn tenant_entities_key(tenant: &str) -> String {
        format!("{}:entities:{tenant}", crate::keys::PREFIX)
    }

    async fn append_with_sequence_guards(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        guards: &[PersistenceSequenceGuard],
    ) -> Result<u64, PersistenceError> {
        if events.is_empty() {
            return Ok(expected_sequence);
        }
        let event_count = u64::try_from(events.len()).map_err(|_| {
            PersistenceError::Storage("Redis append event count exceeds sequence range".to_string())
        })?;
        redis_sequence(expected_sequence, "append precondition")?;
        let expected_new_sequence =
            expected_sequence.checked_add(event_count).ok_or_else(|| {
                PersistenceError::Storage(
                    "event sequence exhausted during Redis append".to_string(),
                )
            })?;
        redis_sequence(expected_new_sequence, "append result")?;

        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let mut keys = vec![
            Self::seq_key(tenant, entity_type, entity_id),
            Self::events_key(tenant, entity_type, entity_id),
            Self::tenant_entities_key(tenant),
            Self::tenant_ordered_entities_key(tenant),
            Self::tenant_ordered_type_entities_key(tenant, entity_type),
        ];

        let mut args: Vec<String> = Vec::with_capacity(events.len() + guards.len() + 3);
        args.push(expected_sequence.to_string());
        args.push(
            serde_json::to_string(&EntityRef {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
            })
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
        );
        args.push(events.len().to_string());

        let mut sequence_nr = expected_sequence;
        for event in events {
            sequence_nr = sequence_nr.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage(
                    "event sequence exhausted during Redis append".to_string(),
                )
            })?;
            let mut stored = event.clone();
            stored.sequence_nr = sequence_nr;
            args.push(
                serde_json::to_string(&stored)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
            );
        }

        for guard in guards {
            let (guard_tenant, guard_type, guard_id) =
                parse_persistence_id_parts(&guard.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            redis_sequence(guard.expected_sequence, "guarded append precondition")?;
            keys.push(Self::seq_key(guard_tenant, guard_type, guard_id));
            args.push(guard.expected_sequence.to_string());
        }

        let result: Vec<i64> = self
            .append_script
            .evalsha_with_reload(&self.client, keys, args)
            .await
            .map_err(storage_error)?;
        match result.as_slice() {
            // Lua is the commit authority. After success there is no separate
            // fallible bookkeeping step that could turn a durable write into
            // an ambiguous error and make the caller retry it.
            [1, new_sequence] => {
                let new_sequence = decoded_redis_sequence(*new_sequence, "append result")?;
                if new_sequence != expected_new_sequence {
                    return Err(PersistenceError::Storage(format!(
                        "Redis append returned sequence {new_sequence}, expected {expected_new_sequence}"
                    )));
                }
                Ok(new_sequence)
            }
            [0, actual] => Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: decoded_redis_sequence(*actual, "append conflict")?,
            }),
            [2, guard_index, actual] => {
                let guard_index = usize::try_from(*guard_index)
                    .ok()
                    .and_then(|index| index.checked_sub(1))
                    .ok_or_else(|| {
                        PersistenceError::Storage(
                            "Redis guarded append returned an invalid guard index".to_string(),
                        )
                    })?;
                let guard = guards.get(guard_index).ok_or_else(|| {
                    PersistenceError::Storage(
                        "Redis guarded append returned an unknown guard index".to_string(),
                    )
                })?;
                Err(PersistenceError::PreconditionFailed {
                    persistence_id: guard.persistence_id.clone(),
                    expected: guard.expected_sequence,
                    actual: decoded_redis_sequence(*actual, "guarded append conflict")?,
                })
            }
            other => Err(PersistenceError::Storage(format!(
                "unexpected Lua script result: {other:?}"
            ))),
        }
    }

    async fn read_events_through_index(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        end_index: i64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let start_index = redis_sequence(from_sequence, "event read")?;
        let encoded_events: Vec<String> = self
            .client
            .lrange(&events_key, start_index, end_index)
            .await
            .map_err(storage_error)?;

        let mut out = Vec::with_capacity(encoded_events.len());
        for encoded in encoded_events {
            let env: PersistenceEnvelope = serde_json::from_str(&encoded)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            redis_sequence(env.sequence_nr, "event decode")?;
            out.push(env);
        }
        out.sort_by_key(|event| event.sequence_nr);
        Ok(out)
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
        self.append_with_sequence_guards(persistence_id, expected_sequence, events, &[])
            .await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        validate_guarded_persistence_append_batch(appends, &[])?;
        let mut non_empty = appends.iter().filter(|append| !append.events.is_empty());
        let first_non_empty = non_empty.next();
        if non_empty.next().is_some() {
            return Err(PersistenceError::Storage(
                "RedisEventStore does not support atomic multi-journal append_batch".to_string(),
            ));
        }
        let committed = match first_non_empty {
            Some(append) => Some((
                append.persistence_id.as_str(),
                self.append(
                    &append.persistence_id,
                    append.expected_sequence,
                    &append.events,
                )
                .await?,
            )),
            None => None,
        };
        Ok(appends
            .iter()
            .map(|append| PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: committed
                    .filter(|(persistence_id, _)| *persistence_id == append.persistence_id)
                    .map_or(append.expected_sequence, |(_, sequence_nr)| sequence_nr),
            })
            .collect())
    }

    async fn append_batch_guarded(
        &self,
        appends: &[PersistenceAppend],
        guards: &[PersistenceSequenceGuard],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        validate_guarded_persistence_append_batch(appends, guards)?;
        if guards.is_empty() {
            return EventStore::append_batch(self, appends).await;
        }
        let mut non_empty = appends.iter().filter(|append| !append.events.is_empty());
        let target = non_empty.next().ok_or_else(|| {
            PersistenceError::Storage(
                "guarded Redis append requires one non-empty target".to_string(),
            )
        })?;
        if non_empty.next().is_some() {
            return Err(PersistenceError::Storage(
                "RedisEventStore does not support guarded multi-journal writes".to_string(),
            ));
        }
        let committed_sequence = self
            .append_with_sequence_guards(
                &target.persistence_id,
                target.expected_sequence,
                &target.events,
                guards,
            )
            .await?;
        Ok(appends
            .iter()
            .map(|append| PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: if append.persistence_id == target.persistence_id {
                    committed_sequence
                } else {
                    append.expected_sequence
                },
            })
            .collect())
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_through_index(persistence_id, from_sequence, -1)
            .await
    }

    async fn read_events_bounded(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let start_index = redis_sequence(from_sequence, "bounded event read")?;
        let additional = i64::try_from(limit - 1).map_err(|_| {
            PersistenceError::Storage("event read limit exceeds Redis range".to_string())
        })?;
        let end_index = start_index.checked_add(additional).ok_or_else(|| {
            PersistenceError::Storage("bounded event read index exceeds Redis range".to_string())
        })?;
        self.read_events_through_index(persistence_id, from_sequence, end_index)
            .await
    }

    async fn read_latest_events(
        &self,
        persistence_ids: &[String],
    ) -> Result<Vec<Option<PersistenceEnvelope>>, PersistenceError> {
        validate_latest_event_batch(persistence_ids)?;
        if persistence_ids.is_empty() {
            return Ok(Vec::new());
        }

        let pipeline = self.client.pipeline();
        for persistence_id in persistence_ids {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
            let events_key = Self::events_key(tenant, entity_type, entity_id);
            let _: () = pipeline
                .lindex(events_key, -1)
                .await
                .map_err(storage_error)?;
        }
        let encoded: Vec<Option<String>> = pipeline.all().await.map_err(storage_error)?;
        if encoded.len() != persistence_ids.len() {
            return Err(PersistenceError::Storage(format!(
                "latest-event pipeline returned {} rows for {} streams",
                encoded.len(),
                persistence_ids.len()
            )));
        }

        let mut out = Vec::with_capacity(encoded.len());
        for encoded_event in encoded {
            match encoded_event {
                Some(encoded_event) => {
                    let event: PersistenceEnvelope = serde_json::from_str(&encoded_event)
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                    redis_sequence(event.sequence_nr, "latest-event decode")?;
                    out.push(Some(event));
                }
                None => {
                    // `None` is part of the shared direct-probe contract. Raw
                    // discovery callers reject it as corruption; derived-index
                    // callers omit it as an orphan. Keep the operation to one
                    // bounded Redis pipeline rather than issuing an N+1
                    // SISMEMBER probe for missing candidates.
                    out.push(None);
                }
            }
        }
        Ok(out)
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        redis_sequence(sequence_nr, "snapshot save")?;
        let key = Self::snapshot_key(tenant, entity_type, entity_id);
        let record = SnapshotRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
        };
        let encoded = serde_json::to_string(&record)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let history_key = Self::snapshot_history_key(tenant, entity_type, entity_id, sequence_nr);
        let history = SnapshotHistoryRecord {
            sequence_nr,
            snapshot: snapshot.to_vec(),
            created_at: chrono::Utc::now(),
        };
        let encoded_history = serde_json::to_string(&history)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let _: i64 = self
            .save_snapshot_script
            .evalsha_with_reload(
                &self.client,
                vec![key, history_key],
                vec![sequence_nr.to_string(), encoded, encoded_history],
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
        redis_sequence(record.sequence_nr, "snapshot load")?;
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

    async fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.list_entity_ids_limited_ordered(tenant, entity_type, limit)
            .await
    }
}

#[cfg(test)]
mod recovery_test;
#[cfg(test)]
mod sequence_test;
#[cfg(test)]
mod tests;
