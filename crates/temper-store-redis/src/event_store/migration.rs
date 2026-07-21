//! Bounded, resumable migration of legacy Redis entity indexes.

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{EntityRef, RedisEventStore};

const ENTITY_EVENT_SCAN_BUDGET: usize = 64;
const ENTITY_REF_SCAN_BUDGET: usize = 64;

/// Migrate at most one bounded journal chunk for one historical entity.
///
/// KEYS[1] = events list, KEYS[2] = tenant live index,
/// KEYS[3] = typed live index, KEYS[4] = tombstone set,
/// KEYS[5] = pending legacy-migration sorted set,
/// KEYS[6] = this entity's durable journal cursor.
/// ARGV[1] = entity_ref_json, ARGV[2] = entity_id, ARGV[3] = event budget.
///
/// Returns `{complete, terminal}`. The pending member is retained until the
/// cursor reaches the journal head, so no partial scan can publish completion.
pub(super) const MIGRATE_ENTITY_LUA: &str = r#"
local entity_ref = ARGV[1]
local entity_id = ARGV[2]
local budget = math.max(1, tonumber(ARGV[3]))
local cursor = tonumber(redis.call('GET', KEYS[6]) or '0')
local terminal = redis.call('SISMEMBER', KEYS[4], entity_ref) == 1
local encoded_events = redis.call('LRANGE', KEYS[1], cursor, cursor + budget - 1)

if not terminal then
    for _, encoded in ipairs(encoded_events) do
        local decoded, event = pcall(cjson.decode, encoded)
        if decoded and type(event) == 'table' then
            local payload = event.payload
            local to_status = nil
            if type(payload) == 'table' and type(payload.to_status) == 'string' then
                to_status = payload.to_status
            end
            if to_status == 'Deleted' or (event.event_type == 'Deleted' and to_status == nil) then
                terminal = true
                break
            end
        end
    end
end

local next_cursor = cursor + #encoded_events
local journal_length = redis.call('LLEN', KEYS[1])
local complete = terminal or next_cursor >= journal_length
if complete then
    if terminal then
        redis.call('SADD', KEYS[4], entity_ref)
        redis.call('ZREM', KEYS[2], entity_ref)
        redis.call('ZREM', KEYS[3], entity_id)
    elseif redis.call('SISMEMBER', KEYS[4], entity_ref) == 0 then
        redis.call('ZADD', KEYS[2], 0, entity_ref)
        redis.call('ZADD', KEYS[3], 0, entity_id)
    end
    -- Retain the classified journal head. Current writers advance it
    -- atomically; a legacy writer leaves it behind, making an update to an
    -- already-known journal observable without rescanning the prefix.
    redis.call('SET', KEYS[6], tostring(journal_length))
    redis.call('ZREM', KEYS[5], entity_ref)
else
    redis.call('SET', KEYS[6], tostring(next_cursor))
end

return {complete and 1 or 0, terminal and 1 or 0}
"#;

/// Park one SSCAN page server-side and return at most one pending reference.
pub(super) const MIGRATE_INDEX_PAGE_LUA: &str = r#"
local budget = math.max(1, tonumber(ARGV[1]))
if redis.call('ZCARD', KEYS[2]) == 0 and redis.call('EXISTS', KEYS[4]) == 0 then
    local cursor = redis.call('GET', KEYS[3]) or '0'
    local page = redis.call('SSCAN', KEYS[1], cursor, 'COUNT', budget)
    redis.call('SET', KEYS[3], page[1])
    if page[1] == '0' then
        redis.call('SET', KEYS[4], '1')
    end
    for _, member in ipairs(page[2]) do
        redis.call('ZADD', KEYS[2], 0, member)
    end
end
return redis.call('ZRANGE', KEYS[2], 0, 0)
"#;

/// Validate or publish the completion marker atomically.
///
/// A mixed-version writer that adds a historical reference without updating
/// the new indexes changes the cardinality equation and invalidates the marker,
/// restarting a bounded SSCAN. The marker proves structural coverage only:
/// listing paths also compare each candidate's classified journal cursor so a
/// legacy update to an existing reference cannot hide behind equal cardinality.
/// Live and tombstone indexes are disjoint.
pub(super) const FINALIZE_ENTITY_INDEX_LUA: &str = r#"
local historical = redis.call('SCARD', KEYS[1])
local covered = redis.call('ZCARD', KEYS[2]) + redis.call('SCARD', KEYS[3])
local pending = redis.call('ZCARD', KEYS[4])
local consistent = historical == covered and pending == 0

if redis.call('EXISTS', KEYS[7]) ~= 0 and not consistent then
    redis.call('DEL', KEYS[7])
    redis.call('DEL', KEYS[6])
    redis.call('SET', KEYS[5], '0')
end

if redis.call('EXISTS', KEYS[7]) ~= 0 and consistent then
    return 1
end

if redis.call('EXISTS', KEYS[6]) ~= 0 and consistent then
    redis.call('SET', KEYS[7], '2')
    return 1
end
return 0
"#;

impl RedisEventStore {
    pub(super) fn decode_entity_refs(
        members: Vec<String>,
    ) -> Result<Vec<EntityRef>, PersistenceError> {
        members
            .into_iter()
            .map(|encoded| {
                serde_json::from_str(&encoded)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))
            })
            .collect()
    }

    pub(super) fn entity_index_event_cursor_key(tenant: &str, encoded_ref: &str) -> String {
        format!(
            "{}:entity_index_event_cursor:{tenant}:{encoded_ref}",
            crate::keys::PREFIX
        )
    }

    async fn migrate_entity_ref(
        &self,
        tenant: &str,
        encoded_ref: String,
        event_budget: usize,
    ) -> Result<(bool, bool), PersistenceError> {
        let entity_ref: EntityRef = serde_json::from_str(&encoded_ref)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        let result: Vec<i64> = self
            .migrate_entity_script
            .evalsha_with_reload(
                &self.client,
                vec![
                    Self::events_key(tenant, &entity_ref.entity_type, &entity_ref.entity_id),
                    Self::tenant_live_entities_key(tenant),
                    Self::typed_live_entities_key(tenant, &entity_ref.entity_type),
                    Self::tenant_tombstones_key(tenant),
                    Self::entity_index_pending_key(tenant),
                    Self::entity_index_event_cursor_key(tenant, &encoded_ref),
                ],
                vec![
                    encoded_ref,
                    entity_ref.entity_id,
                    event_budget.max(1).to_string(),
                ],
            )
            .await
            .map_err(storage_error)?;
        match result.as_slice() {
            [complete, terminal] => Ok((*complete == 1, *terminal == 1)),
            other => Err(PersistenceError::Storage(format!(
                "unexpected Redis entity migration result: {other:?}"
            ))),
        }
    }

    /// Reclassify journal events not observed by the writer that maintained the
    /// live indexes. Exhaustive callers drain all bounded chunks; bounded callers
    /// return a retryable error rather than publishing a partially checked member.
    pub(super) async fn revalidate_live_entity(
        &self,
        tenant: &str,
        entity_ref: &EntityRef,
        exhaustive: bool,
    ) -> Result<bool, PersistenceError> {
        let encoded_ref = serde_json::to_string(entity_ref)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        loop {
            let (complete, terminal) = self
                .migrate_entity_ref(tenant, encoded_ref.clone(), ENTITY_EVENT_SCAN_BUDGET)
                .await?;
            if terminal {
                return Ok(false);
            }
            if complete {
                return Ok(true);
            }
            if !exhaustive {
                return Err(PersistenceError::Storage(
                    "legacy Redis live-entity reclassification incomplete; retry".to_string(),
                ));
            }
        }
    }

    pub(super) async fn revalidate_live_entities(
        &self,
        tenant: &str,
        entity_refs: Vec<EntityRef>,
        exhaustive: bool,
    ) -> Result<Vec<EntityRef>, PersistenceError> {
        let mut live = Vec::with_capacity(entity_refs.len());
        for entity_ref in entity_refs {
            if self
                .revalidate_live_entity(tenant, &entity_ref, exhaustive)
                .await?
            {
                live.push(entity_ref);
            }
        }
        Ok(live)
    }

    async fn validate_or_finalize_entity_index(
        &self,
        tenant: &str,
    ) -> Result<bool, PersistenceError> {
        let complete: i64 = self
            .finalize_entity_index_script
            .evalsha_with_reload(
                &self.client,
                vec![
                    Self::tenant_entities_key(tenant),
                    Self::tenant_live_entities_key(tenant),
                    Self::tenant_tombstones_key(tenant),
                    Self::entity_index_pending_key(tenant),
                    Self::entity_index_cursor_key(tenant),
                    Self::entity_index_scan_complete_key(tenant),
                    Self::entity_index_complete_key(tenant),
                ],
                Vec::<String>::new(),
            )
            .await
            .map_err(storage_error)?;
        Ok(complete == 1)
    }

    #[cfg(test)]
    pub(super) async fn entity_index_is_complete(
        &self,
        tenant: &str,
    ) -> Result<bool, PersistenceError> {
        self.validate_or_finalize_entity_index(tenant).await
    }

    pub(super) async fn ensure_entity_index_complete(
        &self,
        tenant: &str,
    ) -> Result<(), PersistenceError> {
        while !self
            .migrate_entity_index_page(tenant, ENTITY_REF_SCAN_BUDGET)
            .await?
        {}
        Ok(())
    }

    pub(super) async fn migrate_entity_index_page(
        &self,
        tenant: &str,
        budget: usize,
    ) -> Result<bool, PersistenceError> {
        if self.validate_or_finalize_entity_index(tenant).await? {
            return Ok(true);
        }

        let budget = budget.min(u32::MAX as usize).max(1);
        let members: Vec<String> = self
            .migrate_index_page_script
            .evalsha_with_reload(
                &self.client,
                vec![
                    Self::tenant_entities_key(tenant),
                    Self::entity_index_pending_key(tenant),
                    Self::entity_index_cursor_key(tenant),
                    Self::entity_index_scan_complete_key(tenant),
                ],
                vec![budget.to_string()],
            )
            .await
            .map_err(storage_error)?;
        if let Some(encoded_ref) = members.into_iter().next() {
            let _ = self
                .migrate_entity_ref(tenant, encoded_ref, ENTITY_EVENT_SCAN_BUDGET)
                .await?;
        }

        self.validate_or_finalize_entity_index(tenant).await
    }
}
