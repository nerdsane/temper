//! The rows a transition's scheduled actions are written with.
//!
//! A scheduled action (`schedule` or `schedule_at`) is a durable timer: its
//! row is written in the same commit as the event that armed it, so a restart
//! arms it again (`ServerState::rearm_scheduled_actions`). Its id is the
//! arming event's journal sequence and the action's place among the event's
//! schedules, `<seq>.<index>`. The event a fire writes removes the fired
//! row in its own commit, so a row never outlives its fire and a restart
//! cannot fire it again.
//!
//! Arming is idempotent: an action armed for the same time as one of the
//! record's rows still waiting, or twice in one event, keeps the one row
//! ([`EntityActor::arm_scheduled_actions`]). A record that reports progress
//! with the same deadline each time otherwise piled up one row per report,
//! all firing together at the deadline (on 2 Nov 2026 one run fired 20 at
//! once, every one refused).

use chrono::{DateTime, Utc};
use temper_runtime::persistence::ScheduledActionRecord;

use super::actor::EntityActor;
use super::effects::ScheduledAction;
use super::types::{EntityEvent, EntityState};
use crate::request_context::AgentContext;
use crate::request_context::armed_by::encode_scheduled_armed_by;

/// The scheduled-action rows an event changes in its commit.
#[derive(Debug, Default)]
pub(super) struct ScheduledRows {
    /// The rows of the scheduled actions it arms.
    pub(super) armed: Vec<ScheduledActionRecord>,
    /// The arm id of this record's scheduled action it fires.
    pub(super) fired: Option<String>,
}

impl EntityActor {
    /// Name each scheduled action `event` arms, and return the rows to write
    /// with it and the row it fires, if `caller` is firing one of this
    /// record's. An action identical to one already waiting (same action,
    /// same due time) is dropped from `scheduled`, so it is neither written
    /// nor armed again. `state` is the entity before the event is written.
    /// Without a journal nothing is kept and no row is returned; the ids stay
    /// unique in this process.
    pub(super) async fn arm_scheduled_actions(
        &self,
        scheduled: &mut Vec<ScheduledAction>,
        state: &EntityState,
        event: &EntityEvent,
        caller: Option<&AgentContext>,
    ) -> ScheduledRows {
        let journaled = self.event_journal.is_some() && self.event_backend.is_some();
        let sequence = if journaled {
            state.sequence_nr + 1
        } else {
            state.total_event_count as u64
        };
        for action in scheduled.iter_mut() {
            // An absolute schedule keeps its target, even if processing the
            // arming event took time. A relative schedule starts at the event.
            action.due_at = action.due_at.or_else(|| {
                i64::try_from(action.delay_millis)
                    .ok()
                    .and_then(chrono::Duration::try_milliseconds)
                    .and_then(|delay| event.timestamp.checked_add_signed(delay))
            });
        }
        let fired = caller
            .and_then(|caller| caller.fires_scheduled_action.as_ref())
            .filter(|fire| {
                fire.tenant == self.tenant
                    && fire.entity_type == state.entity_type
                    && fire.entity_id == state.entity_id
            })
            .map(|fire| fire.arm_id.clone());
        if journaled && !scheduled.is_empty() {
            let waiting = self.waiting_schedules(state, fired.as_deref()).await;
            drop_identical_schedules(scheduled, waiting);
        }
        for (index, action) in scheduled.iter_mut().enumerate() {
            action.arm_id = format!("{sequence}.{index}");
        }
        if !journaled {
            return ScheduledRows::default();
        }
        if scheduled.is_empty() {
            return ScheduledRows {
                armed: Vec::new(),
                fired,
            };
        }
        let armed = scheduled
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let follow_on = caller
                    .unwrap_or(&AgentContext::default())
                    .for_scheduled_follow_on(&action.action, index, action.delay_millis);
                let armed_by = encode_scheduled_armed_by(&follow_on).unwrap_or_else(|error| {
                    tracing::error!(entity_type = %state.entity_type, entity_id = %state.entity_id,
                        %error, "scheduled action caller not encodable; it fires with no identity");
                    "{}".to_string()
                });
                ScheduledActionRecord {
                    tenant: self.tenant.clone(),
                    entity_type: state.entity_type.clone(),
                    entity_id: state.entity_id.clone(),
                    arm_id: action.arm_id.clone(),
                    action: action.action.clone(),
                    due_at: action
                        .due_at
                        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
                        .to_rfc3339(),
                    armed_by,
                }
            })
            .collect();
        ScheduledRows { armed, fired }
    }

    /// The (action, due time) of each of the record's rows still waiting,
    /// besides `fired`, which this event removes. A journal that cannot be
    /// read gives none, and every action is armed as asked: an arm repeated
    /// then fires once more, and is refused if its record has moved on.
    async fn waiting_schedules(
        &self,
        state: &EntityState,
        fired: Option<&str>,
    ) -> Vec<(String, DateTime<Utc>)> {
        let Some(store) = self.event_journal.as_ref() else {
            return Vec::new();
        };
        match store
            .list_entity_scheduled_actions(&self.tenant, &state.entity_type, &state.entity_id)
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| Some(row.arm_id.as_str()) != fired)
                .filter_map(|row| {
                    let due = DateTime::parse_from_rfc3339(&row.due_at).ok()?;
                    Some((row.action, due.with_timezone(&Utc)))
                })
                .collect(),
            Err(error) => {
                tracing::warn!(tenant = %self.tenant, entity_type = %state.entity_type,
                    entity_id = %state.entity_id, %error,
                    "waiting scheduled actions not readable; arming every one asked for");
                Vec::new()
            }
        }
    }
}

/// Drop from `scheduled` each action identical to one in `waiting` or to one
/// before it: the same action due at the same time. Arms due at different
/// times stay separate.
fn drop_identical_schedules(
    scheduled: &mut Vec<ScheduledAction>,
    mut waiting: Vec<(String, DateTime<Utc>)>,
) {
    scheduled.retain(|action| {
        let due = action.due_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
        let identical = waiting
            .iter()
            .any(|(name, at)| *name == action.action && *at == due);
        if !identical {
            waiting.push((action.action.clone(), due));
        }
        !identical
    });
}
