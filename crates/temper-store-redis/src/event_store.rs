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
    SchemaScope, StreamPublicationFence, scoped_journal_pin_prefix, split_scoped_journal_entity_id,
};
use temper_runtime::persistence::{
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
    storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::keys::{decode_lex_component, encode_lex_component};

#[macro_use]
#[path = "event_store/event_store_core.rs"]
mod event_store_core;
#[macro_use]
#[path = "event_store/event_store_schema.rs"]
mod event_store_schema;

/// Lua script for atomic append: check expected sequence, append events, and index the journal.
///
/// KEYS[1] = seq_key, KEYS[2] = events_key, KEYS[3] = entities_key,
/// KEYS[4] = journals_key, KEYS[5] = unscoped index, KEYS[6] = generation,
/// KEYS[7] = publication fence
/// ARGV[1] = expected_seq (string-encoded integer)
/// ARGV[2] = entity_ref_json (for SADD into entities set)
/// ARGV[3] = journal_member (order-preserving encoded type and id)
/// ARGV[4] = unscoped member or empty, ARGV[5..N] = serialized event JSONs
///
/// Returns: `{1, new_seq}` on success, `{0, current_seq}` on conflict.
const APPEND_LUA: &str = r#"
local seq_key = KEYS[1]
local events_key = KEYS[2]
local entities_key = KEYS[3]
local journals_key = KEYS[4]
local unscoped_index_key = KEYS[5]
local generation_key = KEYS[6]
local fence_key = KEYS[7]
local expected = tonumber(ARGV[1])
local entity_ref = ARGV[2]
local journal_member = ARGV[3]
local unscoped_member = ARGV[4]

local current = tonumber(redis.call('GET', seq_key) or '0')
if current ~= expected then
    return {0, current}
end

if unscoped_member ~= '' then
    local fence_json = redis.call('GET', fence_key)
    if fence_json then
        local fence = cjson.decode(fence_json)
        for i = 5, #ARGV do
            local event = cjson.decode(ARGV[i])
            if event.event_type == fence.publication_action
                and (not event.metadata.kernel or event.metadata.kernel == cjson.null) then
                return {-1, current}
            end
        end
    end
end

for i = 5, #ARGV do
    redis.call('RPUSH', events_key, ARGV[i])
end

local event_count = #ARGV - 4
local new_seq = expected + event_count
redis.call('SET', seq_key, tostring(new_seq))
redis.call('SADD', entities_key, entity_ref)
redis.call('ZADD', journals_key, 0, journal_member)
if unscoped_member ~= '' then
    redis.call('ZADD', unscoped_index_key, 0, unscoped_member)
    redis.call('INCRBY', generation_key, event_count)
end

return {1, new_seq}
"#;

/// Atomically validate generations and replace an installed application's fences.
const ACTIVATE_UNSCOPED_FENCE_LUA: &str = r#"
local current_pointer = redis.call('GET', KEYS[1]) or ''
if current_pointer ~= ARGV[1] then
    return {-2}
end
local binding_count = tonumber(ARGV[2])
for i = 1, binding_count do
    local expected = tonumber(ARGV[2 + (i - 1) * 2 + 1])
    if expected >= 0 then
        local actual = tonumber(redis.call('GET', KEYS[2 + (i - 1) * 2]) or '0')
        if actual ~= expected then
            return {0, i, actual}
        end
    end
end
for i = 1, binding_count do
    local fence_json = ARGV[2 + (i - 1) * 2 + 2]
    local fence_key = KEYS[3 + (i - 1) * 2]
    if fence_json == '' then
        redis.call('DEL', fence_key)
    else
        redis.call('SET', fence_key, fence_json)
    end
end
local new_pointer = ARGV[3 + binding_count * 2]
if new_pointer == '' then
    redis.call('DEL', KEYS[1])
else
    redis.call('SET', KEYS[1], new_pointer)
end
return {1}
"#;

/// Lua script for one atomic, same-tenant multi-journal append.
///
/// Keys are `(sequence, events)` pairs followed by the tenant entity and journal
/// indexes. Arguments are the append count followed by, for each append,
/// `(expected sequence, entity ref, journal member, event count, events...)`.
/// Every optimistic fence is checked before any key is mutated.
const APPEND_BATCH_LUA: &str = r#"
local append_count = tonumber(ARGV[1])
local arg_index = 2
local key_index = 1

for append_index = 1, append_count do
    local expected = tonumber(ARGV[arg_index])
    local unscoped_member = ARGV[arg_index + 3]
    local event_count = tonumber(ARGV[arg_index + 4])
    local current = tonumber(redis.call('GET', KEYS[key_index]) or '0')
    if current ~= expected then
        return {0, append_index, current}
    end
    if unscoped_member ~= '' then
        local fence_json = redis.call('GET', KEYS[key_index + 4])
        if fence_json then
            local fence = cjson.decode(fence_json)
            for event_offset = 1, event_count do
                local event = cjson.decode(ARGV[arg_index + 4 + event_offset])
                if event.event_type == fence.publication_action
                    and (not event.metadata.kernel or event.metadata.kernel == cjson.null) then
                    return {-1, append_index, current}
                end
            end
        end
    end
    arg_index = arg_index + 5 + event_count
    key_index = key_index + 5
end

local entities_key = KEYS[append_count * 5 + 1]
local journals_key = KEYS[append_count * 5 + 2]
local result = {1}
arg_index = 2
key_index = 1

for append_index = 1, append_count do
    local expected = tonumber(ARGV[arg_index])
    local entity_ref = ARGV[arg_index + 1]
    local journal_member = ARGV[arg_index + 2]
    local unscoped_member = ARGV[arg_index + 3]
    local event_count = tonumber(ARGV[arg_index + 4])
    for event_offset = 1, event_count do
        redis.call('RPUSH', KEYS[key_index + 1], ARGV[arg_index + 4 + event_offset])
    end
    local new_seq = expected + event_count
    redis.call('SET', KEYS[key_index], tostring(new_seq))
    redis.call('SADD', entities_key, entity_ref)
    redis.call('ZADD', journals_key, 0, journal_member)
    if unscoped_member ~= '' then
        redis.call('ZADD', KEYS[key_index + 2], 0, unscoped_member)
        redis.call('INCRBY', KEYS[key_index + 3], event_count)
    end
    table.insert(result, new_seq)
    arg_index = arg_index + 5 + event_count
    key_index = key_index + 5
end

return result
"#;

/// Atomically advances the bounded historical-journal index backfill.
const BACKFILL_UNSCOPED_INDEX_LUA: &str = r#"
local current = redis.call('GET', KEYS[1]) or ''
if current ~= ARGV[1] then
    return {0}
end
for i = 4, #ARGV do
    redis.call('ZADD', KEYS[2], 0, ARGV[i])
end
redis.call('SET', KEYS[1], ARGV[2])
if ARGV[3] == '1' then
    redis.call('SET', KEYS[3], '1')
end
return {1}
"#;

/// Redis-backed event store.
#[derive(Clone)]
pub struct RedisEventStore {
    client: Arc<fred::clients::Client>,
    append_script: Script,
    append_batch_script: Script,
    activate_unscoped_fence_script: Script,
    backfill_unscoped_index_script: Script,
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
            append_batch_script: Script::from_lua(APPEND_BATCH_LUA),
            activate_unscoped_fence_script: Script::from_lua(ACTIVATE_UNSCOPED_FENCE_LUA),
            backfill_unscoped_index_script: Script::from_lua(BACKFILL_UNSCOPED_INDEX_LUA),
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

    fn unscoped_journals_key(tenant: &str, entity_type: &str) -> String {
        format!(
            "{}:unscoped_journals:{tenant}:{}",
            crate::keys::PREFIX,
            encode_lex_component(entity_type)
        )
    }

    fn unscoped_index_cursor_key(tenant: &str, entity_type: &str) -> String {
        format!(
            "{}:unscoped_journals_cursor:{tenant}:{}",
            crate::keys::PREFIX,
            encode_lex_component(entity_type)
        )
    }

    fn unscoped_index_complete_key(tenant: &str, entity_type: &str) -> String {
        format!(
            "{}:unscoped_journals_complete:{tenant}:{}",
            crate::keys::PREFIX,
            encode_lex_component(entity_type)
        )
    }

    fn unscoped_generation_key(tenant: &str, entity_type: &str) -> String {
        format!(
            "{}:unscoped_generation:{tenant}:{}",
            crate::keys::PREFIX,
            encode_lex_component(entity_type)
        )
    }

    fn unscoped_fence_key(tenant: &str, entity_type: &str) -> String {
        format!(
            "{}:unscoped_fence:{tenant}:{}",
            crate::keys::PREFIX,
            encode_lex_component(entity_type)
        )
    }

    fn unscoped_application_fence_key(tenant: &str, application_id: &str) -> String {
        format!(
            "{}:unscoped_application_fence:{tenant}:{}",
            crate::keys::PREFIX,
            encode_lex_component(application_id)
        )
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

    async fn update_segment_after_append(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        new_sequence: u64,
    ) -> Result<(), PersistenceError> {
        if new_sequence == expected_sequence {
            return Ok(());
        }
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
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?
            .unwrap_or_else(|| SegmentRecord {
                segment_index,
                start_sequence_nr: (expected_sequence + 1).max(1),
                end_sequence_nr: None,
                snapshot_sequence: None,
                event_count: 0,
                sealed_at: None,
                created_at: chrono::Utc::now(),
            });
        record.end_sequence_nr = Some(new_sequence);
        record.event_count = new_sequence.saturating_sub(record.start_sequence_nr) + 1;
        let encoded = serde_json::to_string(&record)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
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
        Ok(())
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
    redis_event_store_core_methods!();
    redis_event_store_schema_methods!();
}

#[cfg(test)]
#[path = "event_store/tests.rs"]
mod tests;
