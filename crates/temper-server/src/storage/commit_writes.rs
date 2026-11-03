//! A stack's query plane is the rows its event store's commit writes.
//!
//! Every move writes the record's query-plane row and timer row in the commit
//! of its event (`EventStore::commits_entity_rows`), so nothing writes them
//! after the commit and a list read never asks whether a row is missing. A
//! query plane that the journal does not write would be a second copy that
//! nothing keeps up to date, so a stack refuses one: the plane must be the very
//! store the journal appends to, and that store must write its rows in the
//! commit.

use std::sync::Arc;

use super::{QueryPlaneStore, StorageStack};

/// Why a stack refused a query plane.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryPlaneRefused {
    /// The event store does not write a record's rows in its commit.
    #[error("the {backend} event store does not write its query-plane rows in its commit")]
    JournalWritesNoRows {
        /// The stack's backend.
        backend: &'static str,
    },
    /// The query plane is another store than the one the journal appends to.
    #[error("the query plane is not the {backend} event store the journal appends to")]
    NotTheJournal {
        /// The stack's backend.
        backend: &'static str,
    },
}

impl StorageStack {
    /// Read the stack's records from `plane`, which must be the store its
    /// journal appends to.
    ///
    /// # Errors
    ///
    /// [`QueryPlaneRefused`] when the event store does not write its rows in
    /// its commit, or `plane` is another store.
    pub fn with_query_plane(
        mut self,
        plane: Arc<dyn QueryPlaneStore>,
    ) -> Result<Self, QueryPlaneRefused> {
        let backend = self.backend.as_str();
        if !self.events.commits_entity_rows() {
            return Err(QueryPlaneRefused::JournalWritesNoRows { backend });
        }
        if !std::ptr::addr_eq(Arc::as_ptr(&self.events.store), Arc::as_ptr(&plane)) {
            return Err(QueryPlaneRefused::NotTheJournal { backend });
        }
        self.query_plane = Some(plane);
        debug_assert!(self.query_plane_is_journal());
        Ok(self)
    }

    /// The stack without its query plane: reads go to the records' actors.
    pub fn without_query_plane(mut self) -> Self {
        self.query_plane = None;
        self
    }

    /// The store the stack's records are read from, which is the journal's.
    pub fn query_plane(&self) -> Option<&Arc<dyn QueryPlaneStore>> {
        self.query_plane.as_ref()
    }

    /// Read the stack's records through `probe`, a test's wrapper around its
    /// own query plane that steps in between the reads a list makes. The
    /// stack keeps writing through its journal.
    #[cfg(test)]
    pub(crate) fn with_read_probe(mut self, probe: Arc<dyn QueryPlaneStore>) -> Self {
        assert!(
            self.query_plane_is_journal(),
            "a probe wraps the journal's plane"
        );
        self.query_plane = Some(probe);
        self.read_probe = true;
        self
    }

    /// Whether reads go through a test's probe ([`Self::with_read_probe`]).
    pub(crate) fn reads_through_probe(&self) -> bool {
        #[cfg(test)]
        return self.read_probe;
        #[cfg(not(test))]
        false
    }

    /// Whether the query plane is the journal's own store. A stack refuses any
    /// other plane, so this holds whenever the stack has one; it is checked
    /// again where a stack is attached, as the `events` handle is public.
    pub(crate) fn query_plane_is_journal(&self) -> bool {
        self.query_plane.as_ref().is_some_and(|plane| {
            std::ptr::addr_eq(Arc::as_ptr(&self.events.store), Arc::as_ptr(plane))
        })
    }
}
