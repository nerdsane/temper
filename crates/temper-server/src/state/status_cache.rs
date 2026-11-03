//! Each record's newest committed status, for the reads that decide on it
//! without asking the record's actor: cross-entity guards, Cedar context
//! entities, triggers, compensation and the Workspace check before file writes.
//!
//! The writers (a dispatch once its transition is committed, a composite
//! batch, a delete, the backfill of a read) run after awaiting the actor, so
//! they can land in any order. Each entry carries the record's event sequence
//! at the status it holds, and a write replaces it only when it is not older:
//! a slow write that lands after a newer one keeps the newer status.
//!
//! With a journal, a delete writes the tombstone's status, which is newer than
//! any write of the live record, and passivation keeps the entry (the next
//! actor continues the journal's sequence). Without one, a delete and
//! passivation drop the entry: the record's state ends with its actor, and a
//! record created later under the same id counts its events from zero again.
//! An entry the cache evicts or drops is gone, so a late write can fill its
//! place with an older status; with a journal that needs the cache's whole
//! budget of other records written while that one write is in flight.

use chrono::{DateTime, Utc};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use super::ServerState;
use crate::entity_actor::EntityState;

/// How many turns a simulation may give other requests between a status read
/// or commit and its status cache write (see `buggify_yield`).
pub(crate) const STATUS_WRITE_YIELD_BUDGET: u32 = 8;

/// A record's status and where in its history that status was committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedStatus {
    /// The record's status.
    pub status: String,
    /// The record's event sequence at this status ([`EntityState::event_sequence`]).
    pub sequence: u64,
    /// When this entry was written.
    pub updated_at: DateTime<Utc>,
}

/// The cache: "{tenant}:{entity_type}:{entity_id}" to the record's newest status.
pub type StatusCache = lru::LruCache<String, CachedStatus>;

impl ServerState {
    /// Record a committed state of a record as its status, unless the cache
    /// already holds a newer one.
    pub fn cache_entity_status(&self, tenant: &TenantId, state: &EntityState) {
        let key = format!("{tenant}:{}:{}", state.entity_type, state.entity_id);
        let incoming = CachedStatus {
            status: state.status.clone(),
            sequence: state.event_sequence(),
            updated_at: sim_now(),
        };
        let Ok(mut cache) = self.entity_state_cache.lock() else {
            return;
        };
        if cache
            .peek(&key)
            .is_some_and(|held| held.sequence > incoming.sequence)
        {
            return;
        }
        cache.put(key, incoming);
    }

    /// The record's status: the cached one, or else its actor's, which is
    /// then cached.
    #[instrument(skip_all, fields(otel.name = "entity.resolve_entity_status", tenant = %tenant, entity_type, entity_id))]
    pub async fn resolve_entity_status(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<String> {
        self.try_resolve_entity_status(tenant, entity_type, entity_id)
            .await
            .ok()
            .flatten()
    }

    /// Resolve a context record without turning an unreadable record into
    /// an absent status. Authorization must retry a failed read rather than
    /// decide on an incomplete resource view. An ungoverned type has no status.
    pub(crate) async fn try_resolve_entity_status(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<String>, String> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        if let Ok(mut cache) = self.entity_state_cache.lock()
            && let Some(held) = cache.get(&key)
        {
            return Ok(Some(held.status.clone()));
        }

        if !self.is_entity_type_governed(tenant, entity_type)? {
            return Ok(None);
        }
        let response = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await?;
        // Simulation runs other requests between this read and its backfill.
        temper_runtime::buggify::buggify_yield(STATUS_WRITE_YIELD_BUDGET).await;
        self.cache_entity_status(tenant, &response.state);
        Ok(Some(response.state.status))
    }
}
