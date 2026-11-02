//! The query-plane row and timer row a record's event commits with it.
//!
//! The event store keeps them in the journal's transaction
//! (`EventStore::commits_entity_rows`); a stack reads no query plane its
//! journal does not write (`StorageStack::with_query_plane`). Each event
//! writes them as it leaves the record, and nothing after the commit writes
//! them again: a move is one commit, and a caller that hangs up after it
//! cannot leave its rows behind.
//!
//! The timer row follows the same rule a dispatch arms by
//! (`arm_state_timeouts_if_needed`): an event that enters a timed state, or
//! resets its clock, arms that state's timer as its caller; an event that
//! leaves the record where no timer waits (or where a timer that fires once
//! already fired) ends it; any other event leaves the row as it is. A create
//! by a caller is that caller's event, so a record created into a timed state
//! commits its timer row with its `Created` event. An event with no caller (a
//! create by the kernel itself) arms nothing here: the server arms and writes
//! that timer after the commit, and the boot walk arms a record left without
//! one.

use temper_runtime::persistence::{AppendProjection, StateTimerRecord};
use temper_runtime::tenant::TenantId;

use super::state_timeouts::{rearm_clock_start, timer_agent};
use crate::entity_actor::entity_rows::{
    EntityRows, EntityRowsHook, Projection, RowsInput, TimerRow,
};
use crate::odata::read_defaults::ReadDefaults;
use crate::request_context::armed_by::encode_armed_by;
use crate::state::ServerState;

/// The id of the timer row an event that entered or reset `state` at
/// `sequence_nr` arms. The server's armed timer has the same id, so the row
/// it removes when it ends is this one.
pub(crate) fn timer_arm_id(state: &str, sequence_nr: u64) -> String {
    format!("{state}-{sequence_nr}")
}

impl ServerState {
    /// What says which rows an event of `entity_type` writes, for its actor.
    /// The actor asks it only of a store that keeps them with the journal.
    pub(crate) fn entity_rows_hook(&self, tenant: &TenantId, entity_type: &str) -> EntityRowsHook {
        let server = self.clone();
        let (tenant, entity_type) = (tenant.clone(), entity_type.to_string());
        std::sync::Arc::new(move |input: &RowsInput<'_>| {
            server.entity_rows(&tenant, &entity_type, input)
        })
    }

    /// The query-plane row a record's append in a batch commits with its
    /// events, as [`Self::entity_rows`] gives a single append's.
    pub(crate) fn append_projection(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        after: &crate::entity_actor::EntityState,
    ) -> AppendProjection {
        if after.is_tombstoned() {
            return AppendProjection::Remove;
        }
        AppendProjection::Upsert {
            status: after.status.clone(),
            fields: self.query_projection_fields(tenant, entity_type, &after.fields),
            state: self.query_projection_state(after),
        }
    }

    fn entity_rows(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        input: &RowsInput<'_>,
    ) -> EntityRows {
        let after = input.state;
        // Only the kernel's tombstone removes the row; a spec action into a
        // state named Deleted is projected like any other.
        let projection = if input.event.kind == temper_runtime::persistence::EventKind::Tombstone {
            Projection::Remove
        } else {
            Projection::Upsert {
                status: after.status.clone(),
                fields: self.query_projection_fields(tenant, entity_type, &after.fields),
                state: self.query_projection_state(after),
            }
        };
        EntityRows {
            projection: Some(projection),
            state_timer: self.committed_timer_row(tenant, entity_type, input),
        }
    }

    /// The timer row `input`'s event writes, or None to leave it.
    pub(super) fn committed_timer_row(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        input: &RowsInput<'_>,
    ) -> Option<TimerRow> {
        let timeouts = {
            let registry = self.registry.read().ok()?;
            registry
                .get_spec(tenant, entity_type)?
                .automaton
                .state_timeouts
                .clone()
        };
        if timeouts.is_empty() {
            return None;
        }
        let (after, event) = (input.state, input.event);
        let Some(st) = timeouts
            .iter()
            .find(|timeout| timeout.state == after.status)
        else {
            return Some(TimerRow::End);
        };
        let entered = event.from_status != after.status;
        let reset = !entered && st.reset_on.iter().any(|action| action == &event.action);
        if !(entered || reset) {
            // A timer that fires once and has fired wants no row.
            return rearm_clock_start(st, after, event.timestamp)
                .is_none()
                .then_some(TimerRow::End);
        }
        let caller = input.caller?;
        let read =
            ReadDefaults::for_entity(self, tenant, entity_type).fields_as_read(&after.fields);
        let (params, _missing) =
            temper_spec::automaton::resolve_args(&st.args, &after.entity_id, &read);
        let armed_by = match encode_armed_by(&timer_agent(caller)) {
            Ok(armed_by) => armed_by,
            Err(error) => {
                tracing::error!(tenant = %tenant, entity_type, entity_id = %after.entity_id,
                    %error, "state timer caller not encodable; the server writes its row");
                return None;
            }
        };
        let params = serde_json::to_string(&serde_json::Value::Object(params)).ok()?;
        Some(TimerRow::Arm(StateTimerRecord {
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: after.entity_id.clone(),
            state: st.state.clone(),
            arm_id: timer_arm_id(&st.state, after.sequence_nr),
            sequence_nr: after.sequence_nr,
            started_at: event.timestamp.to_rfc3339(),
            params,
            armed_by,
        }))
    }
}
