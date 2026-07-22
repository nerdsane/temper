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

mod atomic;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use fred::prelude::*;
use fred::types::scripts::Script;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use temper_runtime::persistence::STATE_MATERIALIZATION_EVENT_TYPE;
use temper_runtime::persistence::{
    EventStore, IndexReconciliation, JournalBoundary, PersistenceAppend, PersistenceAppendResult,
    PersistenceBatchIdempotency, PersistenceEnvelope, PersistenceError, SnapshotSourceFence,
    is_state_materialization_event_for, storage_error,
};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::parse_persistence_id_parts;

/// Lua script for one atomic journal, entity-catalog, and segment append.
///
/// KEYS: journal sequence, journal events, tenant entities, current-segment
/// pointer, expected active segment, current snapshot, canonical segment zero,
/// the validated materialization-generation marker, first-terminal sequence,
/// and optional batch-idempotency claim.
/// ARGV: expected journal sequence, entity ref, expected segment, timestamp,
/// snapshot-fence mode, exact expected snapshot record, whether to retire the
/// migration snapshot, first terminal sequence in this append (zero if absent),
/// optional batch intent hash, then event records.
///
/// Returns `{1, new_seq}` on success, `{0, current_seq}` on journal conflict,
/// `{2, current_segment}` when the caller must retry its pointer capture, or
/// `{3, current_seq}` when the exact snapshot-generation fence changed.
const APPEND_LUA: &str = r#"
local seq_key = KEYS[1]
local events_key = KEYS[2]
local entities_key = KEYS[3]
local segment_pointer_key = KEYS[4]
local active_segment_key = KEYS[5]
local snapshot_key = KEYS[6]
local canonical_segment_key = KEYS[7]
local materialization_marker_key = KEYS[8]
local terminal_sequence_key = KEYS[9]
local batch_idempotency_key = KEYS[10]
local expected = tonumber(ARGV[1])
local entity_ref = ARGV[2]
local expected_segment = tonumber(ARGV[3])
local timestamp = ARGV[4]
local snapshot_fence_mode = tonumber(ARGV[5])
local expected_snapshot = ARGV[6]
local retire_snapshot = tonumber(ARGV[7])
local batch_intent_hash = ARGV[9]

local function snapshots_equal(left_json, right_json)
    if not left_json or not right_json or right_json == '' then
        return false
    end
    local left = cjson.decode(left_json)
    local right = cjson.decode(right_json)
    return tonumber(left.sequence_nr) == tonumber(right.sequence_nr)
        and cjson.encode(left.snapshot) == cjson.encode(right.snapshot)
end

local current = tonumber(redis.call('GET', seq_key) or '0')
if batch_intent_hash ~= '' then
    local committed_hash = redis.call('GET', batch_idempotency_key)
    if committed_hash then
        if committed_hash == batch_intent_hash then
            return {4, current}
        end
        return {5, current}
    end
end
if current ~= expected then
    return {0, current}
end

local current_snapshot = redis.call('GET', snapshot_key)
if snapshot_fence_mode == 1 and current_snapshot then
    return {3, current}
end
if snapshot_fence_mode == 2
    and not snapshots_equal(current_snapshot, expected_snapshot) then
    return {3, current}
end

local current_segment = tonumber(redis.call('GET', segment_pointer_key) or '0')
if current_segment ~= expected_segment then
    return {2, current_segment}
end

local segment
local target_segment_key
if current == 0 then
    current_segment = 0
    target_segment_key = canonical_segment_key
    segment = {
        segment_index = 0,
        start_sequence_nr = 1,
        end_sequence_nr = cjson.null,
        snapshot_sequence = cjson.null,
        event_count = 0,
        sealed_at = cjson.null,
        created_at = timestamp
    }
else
    target_segment_key = active_segment_key
    local encoded_segment = redis.call('GET', active_segment_key)
    if encoded_segment then
        segment = cjson.decode(encoded_segment)
        local start_sequence = tonumber(segment.start_sequence_nr)
        local invalid_end = segment.end_sequence_nr
            and segment.end_sequence_nr ~= cjson.null
            and tonumber(segment.end_sequence_nr) < start_sequence
        if start_sequence > expected + 1 or invalid_end then
            -- Repair legacy snapshot-ahead topology from the authoritative
            -- journal high-water before assigning the new event.
            current_segment = 0
            target_segment_key = canonical_segment_key
            segment = {
                segment_index = 0,
                start_sequence_nr = 1,
                end_sequence_nr = expected,
                snapshot_sequence = cjson.null,
                event_count = expected,
                sealed_at = cjson.null,
                created_at = timestamp
            }
        end
    else
        current_segment = 0
        target_segment_key = canonical_segment_key
        segment = {
            segment_index = 0,
            start_sequence_nr = 1,
            end_sequence_nr = expected,
            snapshot_sequence = cjson.null,
            event_count = expected,
            sealed_at = cjson.null,
            created_at = timestamp
        }
    end
end

for i = 10, #ARGV do
    redis.call('RPUSH', events_key, ARGV[i])
end

local new_seq = expected + (#ARGV - 9)
segment.end_sequence_nr = new_seq
segment.event_count = math.max(new_seq - tonumber(segment.start_sequence_nr) + 1, 0)

redis.call('SET', seq_key, tostring(new_seq))
local appended_terminal_sequence = tonumber(ARGV[8])
local stored_terminal_sequence = redis.call('GET', terminal_sequence_key)
if not stored_terminal_sequence and current == 0 then
    redis.call('SET', terminal_sequence_key, tostring(appended_terminal_sequence))
elseif stored_terminal_sequence
    and appended_terminal_sequence > 0
    and tonumber(stored_terminal_sequence) == 0 then
    redis.call('SET', terminal_sequence_key, tostring(appended_terminal_sequence))
end
redis.call('SADD', entities_key, entity_ref)
if expected_segment ~= current_segment then
    redis.call('DEL', active_segment_key)
end
redis.call('SET', target_segment_key, cjson.encode(segment))
redis.call('SET', segment_pointer_key, tostring(current_segment))
if retire_snapshot == 1 then
    redis.call('DEL', snapshot_key)
    redis.call('SET', materialization_marker_key, '1')
end
if batch_intent_hash ~= '' then
    redis.call('SET', batch_idempotency_key, batch_intent_hash)
end

return {1, new_seq}
"#;

/// Atomically read a stream's journal and terminal metadata pair.
const READ_BOUNDARY_LUA: &str = r#"
local latest = redis.call('GET', KEYS[1]) or '0'
local terminal = redis.call('GET', KEYS[2]) or ''
return {latest, terminal}
"#;

/// Install lazily reconstructed terminal metadata only if the captured journal
/// high-water is still current and no other migrator already installed it.
const BACKFILL_BOUNDARY_LUA: &str = r#"
local current = redis.call('GET', KEYS[1]) or '0'
if current ~= ARGV[1] then
    return 0
end
if redis.call('EXISTS', KEYS[2]) == 0 then
    redis.call('SET', KEYS[2], ARGV[2])
end
return 1
"#;

/// Atomically retain only a monotonic current snapshot and rotate its segment.
///
/// KEYS: current snapshot, history row, segment pointer, active segment, next
/// segment, journal high-water, the validated materialization marker, and the
/// tenant entity set.
/// ARGV: expected segment index, sequence, snapshot JSON, history JSON, timestamp,
/// snapshot-source fence mode, exact expected source record, and entity ref.
/// Returns `0` on older/identical no-op, `1` on an initial/newer write, `2` on
/// equal-sequence byte replacement, or `-1` when the caller must retry with the
/// new segment pointer. A write rotates only when a journal exists; `-2` rejects
/// a changed exact snapshot source.
const SAVE_SNAPSHOT_LUA: &str = r#"
local expected_segment = tonumber(ARGV[1])
local incoming_sequence = tonumber(ARGV[2])
local incoming_snapshot = ARGV[3]
local current_snapshot = redis.call('GET', KEYS[1])
local same_sequence_rewrite = false
local snapshot_fence_mode = tonumber(ARGV[6])
local expected_snapshot = ARGV[7]

local function snapshot_payloads_equal(left, right)
    return cjson.encode(left.snapshot) == cjson.encode(right.snapshot)
end

local function snapshot_records_equal(left_json, right_json)
    if not left_json or not right_json or right_json == '' then
        return false
    end
    local left = cjson.decode(left_json)
    local right = cjson.decode(right_json)
    return tonumber(left.sequence_nr) == tonumber(right.sequence_nr)
        and snapshot_payloads_equal(left, right)
end

if snapshot_fence_mode == 1 and current_snapshot then
    return -2
end
if snapshot_fence_mode == 2
    and not snapshot_records_equal(current_snapshot, expected_snapshot) then
    return -2
end

local journal_sequence = tonumber(redis.call('GET', KEYS[6]) or '0')
if snapshot_fence_mode == 0
    and redis.call('GET', KEYS[7]) == '1' then
    return 0
end

redis.call('SADD', KEYS[8], ARGV[8])

if current_snapshot then
    local current = cjson.decode(current_snapshot)
    local incoming = cjson.decode(incoming_snapshot)
    local current_sequence = tonumber(current.sequence_nr)
    if current_sequence > incoming_sequence
        or (current_sequence == incoming_sequence and snapshot_payloads_equal(current, incoming)) then
        return 0
    end
    same_sequence_rewrite = current_sequence == incoming_sequence
end

if same_sequence_rewrite then
    redis.call('SET', KEYS[1], incoming_snapshot)
    redis.call('SET', KEYS[2], ARGV[4])
    return 2
end

if journal_sequence == 0 then
    redis.call('SET', KEYS[1], incoming_snapshot)
    redis.call('SET', KEYS[2], ARGV[4])
    return 1
end
if incoming_sequence > journal_sequence then
    redis.call('SET', KEYS[1], incoming_snapshot)
    redis.call('SET', KEYS[2], ARGV[4])
    return 1
end

local current_segment = tonumber(redis.call('GET', KEYS[3]) or '0')
if current_segment ~= expected_segment then
    return -1
end

local timestamp = ARGV[5]
local encoded_segment = redis.call('GET', KEYS[4])
local segment
if encoded_segment then
    segment = cjson.decode(encoded_segment)
else
    segment = {
        segment_index = current_segment,
        start_sequence_nr = 1,
        end_sequence_nr = cjson.null,
        snapshot_sequence = cjson.null,
        event_count = 0,
        sealed_at = cjson.null,
        created_at = timestamp
    }
end

segment.end_sequence_nr = incoming_sequence
segment.snapshot_sequence = incoming_sequence
segment.event_count = math.max(incoming_sequence - tonumber(segment.start_sequence_nr) + 1, 0)
segment.sealed_at = timestamp

local next_segment = {
    segment_index = current_segment + 1,
    start_sequence_nr = incoming_sequence + 1,
    end_sequence_nr = journal_sequence > incoming_sequence and journal_sequence or cjson.null,
    snapshot_sequence = cjson.null,
    event_count = math.max(journal_sequence - incoming_sequence, 0),
    sealed_at = cjson.null,
    created_at = timestamp
}

redis.call('SET', KEYS[1], incoming_snapshot)
redis.call('SET', KEYS[2], ARGV[4])
redis.call('SET', KEYS[4], cjson.encode(segment))
redis.call('SET', KEYS[5], cjson.encode(next_segment))
redis.call('SET', KEYS[3], tostring(current_segment + 1))
return 1
"#;

const JOURNAL_BOUNDARY_PAGE_SIZE: usize = 1_024;
const APPEND_POINTER_RETRY_BUDGET: usize = 8;
const SNAPSHOT_POINTER_RETRY_BUDGET: usize = 8;

/// Redis-backed event store.
#[derive(Clone)]
pub struct RedisEventStore {
    client: Arc<fred::clients::Client>,
    append_script: Script,
    snapshot_script: Script,
    read_boundary_script: Script,
    backfill_boundary_script: Script,
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

#[cfg(test)]
#[derive(Debug, Deserialize)]
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
            snapshot_script: Script::from_lua(SAVE_SNAPSHOT_LUA),
            read_boundary_script: Script::from_lua(READ_BOUNDARY_LUA),
            backfill_boundary_script: Script::from_lua(BACKFILL_BOUNDARY_LUA),
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

    fn terminal_sequence_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
        format!(
            "{}:events_terminal:{tenant}:{entity_type}:{entity_id}",
            crate::keys::PREFIX
        )
    }

    fn snapshot_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
        format!(
            "{}:snapshot:{tenant}:{entity_type}:{entity_id}",
            crate::keys::PREFIX
        )
    }

    fn materialization_marker_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
        format!(
            "{}:state_materialized:{tenant}:{entity_type}:{entity_id}",
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

    fn batch_idempotency_key(claim: Option<&PersistenceBatchIdempotency>) -> String {
        match claim {
            Some(claim) => format!(
                "{}:batch_idempotency:{}:{}:{}:{}",
                crate::keys::PREFIX,
                claim.persistence_id.len(),
                claim.persistence_id,
                claim.idempotency_key.len(),
                claim.idempotency_key
            ),
            None => format!("{}:batch_idempotency:none", crate::keys::PREFIX),
        }
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
    async fn batch_idempotency_committed(
        &self,
        claim: &PersistenceBatchIdempotency,
    ) -> Result<bool, PersistenceError> {
        let key = Self::batch_idempotency_key(Some(claim));
        let committed_hash: Option<String> = self.client.get(&key).await.map_err(storage_error)?;
        let Some(committed_hash) = committed_hash else {
            return Ok(false);
        };
        if committed_hash != claim.intent_hash {
            return Err(PersistenceError::Storage(format!(
                "atomic batch idempotency key '{}' was reused with a different intent",
                claim.idempotency_key
            )));
        }
        Ok(true)
    }

    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_atomically(
            persistence_id,
            expected_sequence,
            events,
            &SnapshotSourceFence::Unchecked,
            None,
        )
        .await
        .map(|(sequence_nr, _)| sequence_nr)
    }

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        _key_rows: &[temper_runtime::persistence::EntityKeyRow],
        _vector_rows: &[temper_runtime::persistence::EntityVectorRow],
        reconciliation: IndexReconciliation,
    ) -> Result<u64, PersistenceError> {
        self.append_atomically(
            persistence_id,
            expected_sequence,
            events,
            &reconciliation.snapshot_source,
            None,
        )
        .await
        .map(|(sequence_nr, _)| sequence_nr)
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        match appends {
            [] => Ok(Vec::new()),
            [append] => {
                let (sequence_nr, batch_already_applied) = self
                    .append_atomically(
                        &append.persistence_id,
                        append.expected_sequence,
                        &append.events,
                        &append.snapshot_source,
                        append.batch_idempotency.as_ref(),
                    )
                    .await?;
                Ok(vec![PersistenceAppendResult {
                    persistence_id: append.persistence_id.clone(),
                    sequence_nr,
                    batch_already_applied,
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

    async fn read_events_page(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        through_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        assert!(limit > 0, "event page limit must be positive");
        assert!(
            through_sequence >= from_sequence,
            "event page boundary must not precede its cursor"
        );
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let events_key = Self::events_key(tenant, entity_type, entity_id);
        let remaining = through_sequence.saturating_sub(from_sequence);
        if remaining == 0 {
            return Ok(Vec::new());
        }
        let page_len = remaining.min(limit as u64);
        let start_index = i64::try_from(from_sequence).map_err(|_| {
            PersistenceError::Storage("event page cursor exceeds Redis list index".to_string())
        })?;
        let end_sequence = from_sequence.saturating_add(page_len);
        let end_index = i64::try_from(end_sequence.saturating_sub(1)).map_err(|_| {
            PersistenceError::Storage("event page boundary exceeds Redis list index".to_string())
        })?;
        let encoded_events: Vec<String> = self
            .client
            .lrange(&events_key, start_index, end_index)
            .await
            .map_err(storage_error)?;

        let mut out = Vec::with_capacity(encoded_events.len());
        for encoded in encoded_events {
            let event = serde_json::from_str::<PersistenceEnvelope>(&encoded)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            out.push(event);
        }
        out.sort_by_key(|event| event.sequence_nr);
        Ok(out)
    }

    async fn journal_boundary(
        &self,
        persistence_id: &str,
    ) -> Result<JournalBoundary, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let sequence_key = Self::seq_key(tenant, entity_type, entity_id);
        let terminal_sequence_key = Self::terminal_sequence_key(tenant, entity_type, entity_id);

        for _attempt in 0..APPEND_POINTER_RETRY_BUDGET {
            let encoded: Vec<String> = self
                .read_boundary_script
                .evalsha_with_reload(
                    &self.client,
                    vec![sequence_key.clone(), terminal_sequence_key.clone()],
                    Vec::<String>::new(),
                )
                .await
                .map_err(storage_error)?;
            let [latest_raw, terminal_raw] = encoded.as_slice() else {
                return Err(PersistenceError::Storage(format!(
                    "unexpected Redis journal boundary result: {encoded:?}"
                )));
            };
            let latest_sequence = latest_raw
                .parse::<u64>()
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            if !terminal_raw.is_empty() {
                let terminal_sequence = terminal_raw
                    .parse::<u64>()
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                if terminal_sequence > latest_sequence {
                    return Err(PersistenceError::Storage(format!(
                        "Redis terminal sequence {terminal_sequence} exceeds journal high-water {latest_sequence} for {persistence_id}"
                    )));
                }
                return Ok(JournalBoundary {
                    latest_sequence,
                    first_terminal_sequence: (terminal_sequence > 0).then_some(terminal_sequence),
                });
            }

            // Compatibility migration for streams written before terminal
            // metadata existed. Scan only the captured high-water, then install
            // the result with a high-water CAS so every later lookup is O(1).
            let mut cursor = 0_u64;
            let mut first_terminal_sequence = None;
            while cursor < latest_sequence && first_terminal_sequence.is_none() {
                let remaining = latest_sequence - cursor;
                let page_len = usize::try_from(remaining.min(JOURNAL_BOUNDARY_PAGE_SIZE as u64))
                    .expect("bounded Redis journal page length fits usize");
                let page = self
                    .read_events_page(persistence_id, cursor, latest_sequence, page_len)
                    .await?;
                if page.len() != page_len {
                    return Err(PersistenceError::Storage(format!(
                        "Redis journal boundary expected {page_len} events after sequence {cursor}, received {}",
                        page.len()
                    )));
                }
                for (offset, event) in page.iter().enumerate() {
                    let expected_sequence = cursor + offset as u64 + 1;
                    if event.sequence_nr != expected_sequence {
                        return Err(PersistenceError::Storage(format!(
                            "Redis journal boundary expected sequence {expected_sequence}, received {}",
                            event.sequence_nr
                        )));
                    }
                    if event.transitions_to_deleted() {
                        first_terminal_sequence = Some(event.sequence_nr);
                        break;
                    }
                }
                cursor = page
                    .last()
                    .map(|event| event.sequence_nr)
                    .expect("validated non-empty Redis journal page");
            }
            let installed: i64 = self
                .backfill_boundary_script
                .evalsha_with_reload(
                    &self.client,
                    vec![sequence_key.clone(), terminal_sequence_key.clone()],
                    vec![
                        latest_sequence.to_string(),
                        first_terminal_sequence.unwrap_or(0).to_string(),
                    ],
                )
                .await
                .map_err(storage_error)?;
            if installed == 1 {
                return Ok(JournalBoundary {
                    latest_sequence,
                    first_terminal_sequence,
                });
            }
        }

        Err(PersistenceError::Storage(format!(
            "Redis journal boundary for {persistence_id} did not stabilize after {APPEND_POINTER_RETRY_BUDGET} attempts"
        )))
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.save_snapshot_atomically(
            persistence_id,
            sequence_nr,
            snapshot,
            &SnapshotSourceFence::Unchecked,
        )
        .await
    }

    async fn save_snapshot_if_source(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &SnapshotSourceFence,
        _key_contract: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.save_snapshot_atomically(persistence_id, sequence_nr, snapshot, source)
            .await
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
}
