//! The timer rows of the state-timer simulation: they outlive one server
//! process.

use std::collections::BTreeMap;
use std::sync::Mutex;

use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::DeterministicRng;
use temper_server::storage::StateTimerStore;

/// Timer rows, kept the way the SQL stores keep them: one per record, a row
/// armed at a lower journal sequence never replacing a higher one, removed
/// only by the arming that wrote it. Each write lands after a seeded number
/// of scheduler turns, so writes of overlapping arms and cancellations land
/// in any order, as round trips to a real store do.
pub(crate) struct SimTimerRows {
    rows: Mutex<BTreeMap<(String, String, String), StateTimerRecord>>,
    rng: Mutex<DeterministicRng>,
    pub(super) listing_failures: Mutex<BTreeMap<String, usize>>,
    /// A write takes fewer scheduler turns than this to land.
    round_trip_turns: usize,
}

impl SimTimerRows {
    /// No rows, with write delays of up to 7 turns drawn from `seed`.
    pub(crate) fn new(seed: u64) -> Self {
        Self::with_round_trips(seed, 8)
    }

    /// No rows, with write delays of fewer than `turns` turns drawn from
    /// `seed`: longer writes overlap more of what runs beside them.
    pub(crate) fn with_round_trips(seed: u64, turns: usize) -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            rng: Mutex::new(DeterministicRng::new(seed ^ 0x7177_e125)),
            listing_failures: Mutex::new(BTreeMap::new()),
            round_trip_turns: turns,
        }
    }

    /// The record's row now.
    pub(crate) fn row(&self, tenant: &str, id: &str) -> Option<StateTimerRecord> {
        let rows = self.rows.lock().unwrap();
        rows.get(&(tenant.to_string(), "Job".to_string(), id.to_string()))
            .cloned()
    }

    /// Wait the seeded number of scheduler turns a write takes to land.
    async fn round_trip(&self) {
        let turns = self.rng.lock().unwrap().next_bound(self.round_trip_turns);
        for _ in 0..turns {
            tokio::task::yield_now().await;
        }
    }
}

#[async_trait::async_trait]
impl StateTimerStore for SimTimerRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        let key = (
            timer.tenant.clone(),
            timer.entity_type.clone(),
            timer.entity_id.clone(),
        );
        self.round_trip().await;
        let mut rows = self.rows.lock().unwrap();
        if rows
            .get(&key)
            .is_none_or(|row| timer.sequence_nr >= row.sequence_nr)
        {
            rows.insert(key, timer.clone());
        }
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let key = (
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        self.round_trip().await;
        let mut rows = self.rows.lock().unwrap();
        if rows.get(&key).is_some_and(|row| row.arm_id == arm_id) {
            rows.remove(&key);
        }
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        let fail = {
            let mut failures = self.listing_failures.lock().unwrap();
            let remaining = failures.entry(tenant.to_string()).or_default();
            let fail = *remaining > 0;
            *remaining = remaining.saturating_sub(1);
            fail
        };
        if fail {
            self.round_trip().await;
            return Err(PersistenceError::Storage(
                "injected timer listing failure".to_string(),
            ));
        }
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .values()
            .filter(|row| row.tenant == tenant)
            .cloned()
            .collect())
    }
}
