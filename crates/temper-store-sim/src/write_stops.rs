//! A store that stops taking writes, then takes them again: a disk that
//! fills, or a database that refuses every write, while reads go on working.

use temper_runtime::persistence::PersistenceError;

use super::{SimEventStore, SimEventStoreInner};

/// Whether the store takes writes.
#[derive(Debug, Default)]
pub(super) enum Writes {
    /// Every append is written.
    #[default]
    Taken,
    /// Appends are written until one writes this journal; then none is.
    StopAfter(String),
    /// No append is written.
    Stopped,
}

impl SimEventStore {
    /// Stop taking writes the moment the next append that writes
    /// `persistence_id`, alone or in a batch, lands. From then on every
    /// append, to any journal, fails with a storage error and writes nothing,
    /// until [`Self::take_writes_again`]; reads go on working. Set before a
    /// move, it is a store that goes down right after the move commits, so
    /// whatever that commit sets off meets a store that takes no writes.
    pub fn stop_writes_after_next_append(&self, persistence_id: &str) {
        self.lock().writes = Writes::StopAfter(persistence_id.to_string());
    }

    /// Take writes again, and cancel a stop that has not happened yet.
    pub fn take_writes_again(&self) {
        self.lock().writes = Writes::Taken;
    }

    /// How many appends that would have written `persistence_id`, alone or
    /// in a batch, the store has refused because it took no writes. A test
    /// reads it to know a write it expects to meet the refusal did.
    pub fn refused_appends(&self, persistence_id: &str) -> u64 {
        self.lock()
            .refused_appends
            .get(persistence_id)
            .copied()
            .unwrap_or(0)
    }
}

impl SimEventStoreInner {
    /// The error an append that writes `persistence_ids` meets while the
    /// store takes no writes, counted against each of them.
    pub(super) fn refused_write<'a>(
        &mut self,
        persistence_ids: impl IntoIterator<Item = &'a str>,
    ) -> Option<PersistenceError> {
        if !matches!(self.writes, Writes::Stopped) {
            return None;
        }
        for persistence_id in persistence_ids {
            *self
                .refused_appends
                .entry(persistence_id.to_string())
                .or_default() += 1;
        }
        Some(PersistenceError::Storage(
            "SimEventStore: the store takes no writes".to_string(),
        ))
    }

    /// An append wrote `persistence_id`: stop taking writes if the store was
    /// set to stop after it.
    pub(super) fn wrote(&mut self, persistence_id: &str) {
        if matches!(&self.writes, Writes::StopAfter(id) if id == persistence_id) {
            self.writes = Writes::Stopped;
        }
    }
}
