//! What the sim store does with a move's commit, drawn from the seed.

use std::time::Duration;

use temper_runtime::scheduler::DeterministicRng;
use temper_store_sim::SimEventStore;

/// What the store does with a move's commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    None,
    FailBeforeWriting,
    LandThenFail,
    LandThenConflict,
    ConflictBeforeWriting,
    LandLate,
    LandThenFailUnreadable,
}

impl Fault {
    pub fn draw(rng: &mut DeterministicRng) -> Self {
        match rng.next_bound(10) {
            0 => Self::FailBeforeWriting,
            1 => Self::LandThenFail,
            2 => Self::LandThenConflict,
            3 => Self::ConflictBeforeWriting,
            4 => Self::LandLate,
            5 => Self::LandThenFailUnreadable,
            _ => Self::None,
        }
    }

    pub fn apply(self, store: &SimEventStore, journal: &str) {
        match self {
            Self::None => {}
            Self::FailBeforeWriting => store.fail_next_appends(journal, 1),
            Self::LandThenFail => store.land_next_appends_then_fail(journal, 1),
            Self::LandThenConflict => store.lose_next_append_answers(journal, 1),
            Self::ConflictBeforeWriting => store.inject_concurrency_violations(journal, 1),
            Self::LandLate => store.inject_append_delay(journal, Duration::from_millis(40)),
            Self::LandThenFailUnreadable => {
                store.land_next_appends_then_fail(journal, 1);
                store.fail_next_reads(journal, 1);
            }
        }
    }

    /// Clear what the move did not use up, so it cannot reach the next move.
    pub fn clear(store: &SimEventStore, journal: &str) {
        store.fail_next_appends(journal, 0);
        store.land_next_appends_then_fail(journal, 0);
        store.lose_next_append_answers(journal, 0);
        store.inject_concurrency_violations(journal, 0);
        store.fail_next_reads(journal, 0);
    }
}
