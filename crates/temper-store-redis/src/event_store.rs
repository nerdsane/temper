//! Redis-backed implementation of the [`EventStore`] trait.
//!
//! Uses Redis primitives:
//! - `LIST` per entity for ordered event journal entries
//! - `STRING` per entity for latest sequence number
//! - `STRING` per entity for snapshots
//! - `SET` per tenant to track historical `(entity_type, entity_id)` pairs
//! - `ZSET` live-entity indexes maintained atomically with journal appends
//!
//! The `append()` operation uses a Lua script (`EVALSHA`) to atomically
//! check-and-set the sequence number, preventing lost-update races between
//! concurrent writers on the same entity.

use std::sync::Arc;

use fred::prelude::*;
use fred::types::scripts::Script;
use serde::{Deserialize, Serialize};
use temper_runtime::persistence::{
    EventStore, JournalRead, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError, storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;

mod backend;
mod migration;
mod segments;

/// Lua script for atomic append and live-entity index maintenance.
///
/// KEYS[1] = seq_key, KEYS[2] = events_key, KEYS[3] = entities_key,
/// KEYS[4] = tenant live index, KEYS[5] = typed live index,
/// KEYS[6] = tombstone set, KEYS[7] = index-complete marker,
/// KEYS[8] = this entity's last classified journal sequence,
/// KEYS[9] = entity references discovered by the current index protocol
/// ARGV[1] = expected_seq (string-encoded integer)
/// ARGV[2] = entity_ref_json (for SADD into entities set)
/// ARGV[3] = entity_id, ARGV[4] = whether this append contains a tombstone
/// ARGV[5..N] = serialized event JSONs
///
/// Returns: `{1, new_seq}` on success, `{0, current_seq}` on conflict.
const APPEND_LUA: &str = r#"
local seq_key = KEYS[1]
local events_key = KEYS[2]
local entities_key = KEYS[3]
local live_entities_key = KEYS[4]
local typed_live_entities_key = KEYS[5]
local tombstones_key = KEYS[6]
local index_complete_key = KEYS[7]
local classified_sequence_key = KEYS[8]
local discovered_entities_key = KEYS[9]
local expected = tonumber(ARGV[1])
local entity_ref = ARGV[2]
local entity_id = ARGV[3]
local is_tombstone = ARGV[4] == '1'

local current = tonumber(redis.call('GET', seq_key) or '0')
if current ~= expected then
    return {0, current}
end

local new_tenant = redis.call('SCARD', entities_key) == 0

for i = 5, #ARGV do
    redis.call('RPUSH', events_key, ARGV[i])
end

local new_seq = expected + (#ARGV - 4)
redis.call('SET', seq_key, tostring(new_seq))
redis.call('SADD', entities_key, entity_ref)

if is_tombstone then
    redis.call('SADD', tombstones_key, entity_ref)
    redis.call('ZREM', live_entities_key, entity_ref)
    redis.call('ZREM', typed_live_entities_key, entity_id)
elseif redis.call('SISMEMBER', tombstones_key, entity_ref) == 0 then
    redis.call('ZADD', live_entities_key, 0, entity_ref)
    redis.call('ZADD', typed_live_entities_key, 0, entity_id)
end

-- Current writers classify liveness in the same transaction as the journal
-- append. Legacy writers do not advance this cursor, so readers can detect and
-- reclassify only the journal suffix written during a mixed-version rollout.
-- Advance only from an exact classified head. A missing or stale cursor means
-- a legacy writer may have added an unclassified suffix that must remain
-- visible to readers even if this current writer appends again first.
local classified = tonumber(redis.call('GET', classified_sequence_key) or '-1')
if current == 0 or classified == current then
    redis.call('SET', classified_sequence_key, tostring(new_seq))
    redis.call('SADD', discovered_entities_key, entity_ref)
end

-- The first append creates a complete index. A pre-existing historical set
-- without this marker is a legacy tenant and is migrated lazily by readers.
if new_tenant and redis.call('EXISTS', index_complete_key) == 0 then
    redis.call('SET', index_complete_key, '1')
end

return {1, new_seq}
"#;

/// Atomically publish one snapshot as both the current value and its history row.
///
/// KEYS[1] = current snapshot key, KEYS[2] = same-sequence history key
/// ARGV[1] = current snapshot record, ARGV[2] = history record
const SAVE_SNAPSHOT_LUA: &str = r#"
redis.call('SET', KEYS[2], ARGV[2])
redis.call('SET', KEYS[1], ARGV[1])
return 1
"#;

/// Atomically replace the payload for one already-persisted snapshot boundary.
///
/// KEYS[1] = current snapshot key, KEYS[2] = same-sequence history key
/// ARGV[1] = exact raw current snapshot record, ARGV[2] = replacement history,
/// ARGV[3] = replacement current snapshot record
///
/// Returns `1` when replaced and `0` if the current snapshot changed.
const REPLACE_SNAPSHOT_LUA: &str = r#"
if redis.call('GET', KEYS[1]) ~= ARGV[1] then
    return 0
end

redis.call('SET', KEYS[2], ARGV[2])
redis.call('SET', KEYS[1], ARGV[3])
return 1
"#;

/// Read the durable head and requested tail atomically.
///
/// KEYS[1] = sequence key, KEYS[2] = events list
/// ARGV[1] = zero-based list index corresponding to `from_sequence`
///
/// Returns the head as the first string followed by serialized events.
const READ_EVENTS_WITH_HEAD_LUA: &str = r#"
local result = {tostring(redis.call('GET', KEYS[1]) or '0')}
local events = redis.call('LRANGE', KEYS[2], tonumber(ARGV[1]), -1)
for _, event in ipairs(events) do
    table.insert(result, event)
end
return result
"#;

/// Redis-backed event store.
#[derive(Clone)]
pub struct RedisEventStore {
    client: Arc<fred::clients::Client>,
    append_script: Script,
    migrate_entity_script: Script,
    migrate_index_page_script: Script,
    finalize_entity_index_script: Script,
    read_events_with_head_script: Script,
    replace_snapshot_script: Script,
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
struct SegmentRecord {
    segment_index: u64,
    start_sequence_nr: u64,
    end_sequence_nr: Option<u64>,
    snapshot_sequence: Option<u64>,
    event_count: u64,
    sealed_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct EntityRef {
    entity_type: String,
    entity_id: String,
}

fn is_entity_tombstone(event: &PersistenceEnvelope) -> bool {
    match event
        .payload
        .get("to_status")
        .and_then(serde_json::Value::as_str)
    {
        Some(status) => status == "Deleted",
        None => event.event_type == "Deleted",
    }
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
            migrate_entity_script: Script::from_lua(migration::MIGRATE_ENTITY_LUA),
            migrate_index_page_script: Script::from_lua(migration::MIGRATE_INDEX_PAGE_LUA),
            finalize_entity_index_script: Script::from_lua(migration::FINALIZE_ENTITY_INDEX_LUA),
            read_events_with_head_script: Script::from_lua(READ_EVENTS_WITH_HEAD_LUA),
            replace_snapshot_script: Script::from_lua(REPLACE_SNAPSHOT_LUA),
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

    fn tenant_live_entities_key(tenant: &str) -> String {
        format!("{}:live_entities:{tenant}", crate::keys::PREFIX)
    }

    fn typed_live_entities_key(tenant: &str, entity_type: &str) -> String {
        format!(
            "{}:live_entities:{tenant}:{entity_type}",
            crate::keys::PREFIX
        )
    }

    fn tenant_tombstones_key(tenant: &str) -> String {
        format!("{}:entity_tombstones:{tenant}", crate::keys::PREFIX)
    }

    fn entity_index_complete_key(tenant: &str) -> String {
        format!("{}:entity_index_version:{tenant}", crate::keys::PREFIX)
    }

    fn entity_index_cursor_key(tenant: &str) -> String {
        format!("{}:entity_index_cursor:{tenant}", crate::keys::PREFIX)
    }

    fn entity_index_pending_key(tenant: &str) -> String {
        format!("{}:entity_index_pending:{tenant}", crate::keys::PREFIX)
    }

    fn entity_index_scan_complete_key(tenant: &str) -> String {
        format!(
            "{}:entity_index_scan_complete:{tenant}",
            crate::keys::PREFIX
        )
    }

    fn entity_index_discovered_key(tenant: &str) -> String {
        format!("{}:entity_index_discovered:{tenant}", crate::keys::PREFIX)
    }

    fn entity_index_scan_spill_key(tenant: &str) -> String {
        format!("{}:entity_index_scan_spill:{tenant}", crate::keys::PREFIX)
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

#[cfg(test)]
#[path = "event_store_snapshot_test.rs"]
mod snapshot_test;

#[cfg(test)]
#[path = "event_store/tests/mod.rs"]
mod tests;
