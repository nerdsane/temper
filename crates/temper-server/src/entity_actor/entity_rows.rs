//! The rows a record keeps beside its journal, written with its events.
//!
//! A store that writes them in the journal's transaction
//! (`EventStore::commits_entity_rows`) gets, with each event, the record's
//! query-plane row and its timer row as the event leaves the record. Which
//! rows those are depends on the record's spec as the server holds it (the
//! fields the query plane leaves out, the state timeouts), so the server
//! gives each actor an [`EntityRowsHook`] that says.

use std::sync::Arc;

use temper_runtime::persistence::{CommitRows, ProjectionRow};

use super::types::{EntityEvent, EntityState};
use crate::request_context::AgentContext;

/// What an event leaves the record as, for its rows.
pub(crate) struct RowsInput<'a> {
    /// The record after the event, at the event's sequence.
    pub(crate) state: &'a EntityState,
    /// The event.
    pub(crate) event: &'a EntityEvent,
    /// Who sent it, or created the record for a `Created` event. None: an
    /// internal write (a field update, a delete, a create by the kernel)
    /// with no caller to arm a timer as.
    pub(crate) caller: Option<&'a AgentContext>,
}

/// The rows an event writes in its commit.
#[derive(Debug, Default)]
pub(crate) struct EntityRows {
    pub(crate) projection: Option<Projection>,
    pub(crate) state_timer: Option<TimerRow>,
}

/// The record's query-plane row.
#[derive(Debug)]
pub(crate) enum Projection {
    Upsert {
        status: String,
        fields: serde_json::Value,
        state: serde_json::Value,
    },
    Remove,
}

/// The record's timer mutation, shared with atomic batch appends.
pub(crate) use temper_runtime::persistence::AppendStateTimer as TimerRow;

/// Says which rows an event writes.
pub(crate) type EntityRowsHook = Arc<dyn Fn(&RowsInput<'_>) -> EntityRows + Send + Sync>;

impl EntityRows {
    /// The rows as [`CommitRows`] borrows them.
    pub(super) fn commit_rows<'a>(&'a self, rows: CommitRows<'a>) -> CommitRows<'a> {
        CommitRows {
            projection: self.projection.as_ref().map(|projection| match projection {
                Projection::Upsert {
                    status,
                    fields,
                    state,
                } => ProjectionRow::Upsert {
                    status,
                    fields,
                    state,
                },
                Projection::Remove => ProjectionRow::Remove,
            }),
            state_timer: self.state_timer.as_ref().map(TimerRow::as_row),
            ..rows
        }
    }
}
