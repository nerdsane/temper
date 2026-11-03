//! The rows an entity keeps beside its journal: its query-plane row and its
//! state-timer row, and a count of the commits that wrote it.
//!
//! An append writes them under the same lock as the journal when its
//! [`CommitRows`] carries them, as a real store writes them in the journal's
//! transaction. A timer row can also be written on its own
//! ([`SimEventStore::upsert_state_timer`]), as a server that does not commit
//! it with the event does; that is a commit of its own, and counted as one.

use temper_runtime::persistence::{CommitRows, ProjectionRow, StateTimerRecord, StateTimerRow};
use temper_runtime::tenant::parse_persistence_id_parts;

use super::{SimEventStore, SimEventStoreInner};

/// An entity's query-plane row.
#[derive(Debug, Clone, PartialEq)]
pub struct SimProjection {
    /// The entity's status.
    pub status: String,
    /// The projected fields.
    pub fields: serde_json::Value,
    /// The journal sequence the row reflects.
    pub sequence_nr: u64,
}

type EntityKey = (String, String, String);

fn entity_key(persistence_id: &str) -> Option<EntityKey> {
    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(persistence_id).ok()?;
    Some((
        tenant.to_string(),
        entity_type.to_string(),
        entity_id.to_string(),
    ))
}

fn timer_key(row: &StateTimerRecord) -> EntityKey {
    (
        row.tenant.clone(),
        row.entity_type.clone(),
        row.entity_id.clone(),
    )
}

impl SimEventStoreInner {
    /// Write `rows`' entity rows for `persistence_id`, whose journal now ends
    /// at `sequence_nr`, in the commit that just wrote its events.
    pub(super) fn commit_entity_rows(
        &mut self,
        persistence_id: &str,
        sequence_nr: u64,
        rows: &CommitRows<'_>,
    ) {
        *self.commits.entry(persistence_id.to_string()).or_default() += 1;
        let Some(key) = entity_key(persistence_id) else {
            return;
        };
        match rows.projection {
            Some(ProjectionRow::Upsert { status, fields, .. }) => {
                self.projections.insert(
                    persistence_id.to_string(),
                    SimProjection {
                        status: status.to_string(),
                        fields: fields.clone(),
                        sequence_nr,
                    },
                );
            }
            Some(ProjectionRow::Remove) => {
                self.projections.remove(persistence_id);
            }
            None => {}
        }
        match rows.state_timer {
            Some(StateTimerRow::Arm(row)) => self.keep_newer_timer(row),
            Some(StateTimerRow::End) => {
                self.state_timers.remove(&key);
            }
            None => {}
        }
    }

    /// Keep `row` unless the entity's row is from a later sequence.
    fn keep_newer_timer(&mut self, row: &StateTimerRecord) {
        let key = timer_key(row);
        if self
            .state_timers
            .get(&key)
            .is_none_or(|kept| row.sequence_nr >= kept.sequence_nr)
        {
            self.state_timers.insert(key, row.clone());
        }
    }
}

impl SimEventStore {
    /// The entity's query-plane row, if it has one.
    pub fn projection(&self, persistence_id: &str) -> Option<SimProjection> {
        self.lock().projections.get(persistence_id).cloned()
    }

    /// The entity's state-timer row, if it has one.
    pub fn state_timer(&self, persistence_id: &str) -> Option<StateTimerRecord> {
        let key = entity_key(persistence_id)?;
        self.lock().state_timers.get(&key).cloned()
    }

    /// How many commits have written this entity's journal or its rows.
    pub fn commits(&self, persistence_id: &str) -> u64 {
        self.lock()
            .commits
            .get(persistence_id)
            .copied()
            .unwrap_or(0)
    }

    /// Write a timer row on its own: one commit. A row from an earlier
    /// sequence than the entity's row is not kept.
    pub fn upsert_state_timer(&self, row: &StateTimerRecord) {
        let mut inner = self.lock();
        let pid = format!("{}:{}:{}", row.tenant, row.entity_type, row.entity_id);
        *inner.commits.entry(pid).or_default() += 1;
        inner.keep_newer_timer(row);
    }

    /// Remove the entity's timer row if it holds `arm_id`: one commit.
    pub fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) {
        let mut inner = self.lock();
        let pid = format!("{tenant}:{entity_type}:{entity_id}");
        *inner.commits.entry(pid).or_default() += 1;
        let key = (
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        if inner
            .state_timers
            .get(&key)
            .is_some_and(|row| row.arm_id == arm_id)
        {
            inner.state_timers.remove(&key);
        }
    }

    /// Every timer row of `tenant`.
    pub fn list_state_timers(&self, tenant: &str) -> Vec<StateTimerRecord> {
        self.lock()
            .state_timers
            .values()
            .filter(|row| row.tenant == tenant)
            .cloned()
            .collect()
    }
}
