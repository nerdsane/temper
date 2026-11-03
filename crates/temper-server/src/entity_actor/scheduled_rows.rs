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
//!
//! The same event writes the deadline row of each background module call it
//! starts that names an `on_failure` (`request_context::calls`), with the id
//! `<seq>.call:<integration>`; the call's answer removes it in its commit.
//! A call's deadline is its own: it never makes a schedule identical to it,
//! and is never dropped as identical to one. A schedule kept only because a
//! deadline looked the same would vanish when the call answered.

use chrono::{DateTime, Utc};
use temper_runtime::persistence::ScheduledActionRecord;

use super::actor::EntityActor;
use super::effects::ScheduledAction;
use super::types::{EntityEvent, EntityState};
use crate::request_context::AgentContext;
use crate::request_context::armed_by::{encode_call_armed_by, encode_scheduled_armed_by};
use crate::request_context::calls::{call_arm_id, called_integration};

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
    /// with it: those actions' and the deadlines of the background calls its
    /// `custom_effects` start, and the row it removes, if `caller` fires one
    /// of this record's or answers one of its calls. An action identical to
    /// a schedule already waiting (same action, same due time) is dropped
    /// from `scheduled`, so it is neither written nor armed again. `state` is
    /// the entity before the event is written. Without a journal nothing is
    /// kept and no row is returned; the ids stay unique in this process.
    pub(super) async fn arm_scheduled_actions(
        &self,
        scheduled: &mut Vec<ScheduledAction>,
        custom_effects: &[String],
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
        // An answer fires no scheduled action (`AgentContext::for_answer`),
        // so a move removes at most one row.
        let fired = caller
            .and_then(|caller| {
                caller
                    .fires_scheduled_action
                    .as_ref()
                    .or(caller.ends_call.as_ref())
            })
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
        let mut armed: Vec<ScheduledActionRecord> = scheduled
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
        if let Some(caller) = caller {
            armed.extend(self.call_deadlines(sequence, state, event, custom_effects, caller));
        }
        ScheduledRows { armed, fired }
    }

    /// The deadline rows of the background calls `custom_effects` start that
    /// name an `on_failure`, from those `caller` says the move may start:
    /// each answers its call with that action, as the call's own answer
    /// would be made, once the call's deadline has passed since `event`.
    fn call_deadlines(
        &self,
        sequence: u64,
        state: &EntityState,
        event: &EntityEvent,
        custom_effects: &[String],
        caller: &AgentContext,
    ) -> Vec<ScheduledActionRecord> {
        let mut rows: Vec<ScheduledActionRecord> = Vec::new();
        for effect in custom_effects {
            let Some(call) = caller
                .background_calls
                .iter()
                .find(|call| call.trigger == *effect)
            else {
                continue;
            };
            let arm_id = call_arm_id(sequence, &call.integration);
            if rows.iter().any(|row| row.arm_id == arm_id) {
                continue;
            }
            let due_at = chrono::Duration::from_std(call.deadline)
                .ok()
                .and_then(|deadline| event.timestamp.checked_add_signed(deadline))
                .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC);
            let answer = caller.for_call_answer(&call.integration);
            let armed_by = encode_call_armed_by(&answer, call.process_id).unwrap_or_else(|error| {
                tracing::error!(entity_type = %state.entity_type, entity_id = %state.entity_id,
                    %error, "call deadline caller not encodable; it fires with no identity");
                "{}".to_string()
            });
            rows.push(ScheduledActionRecord {
                tenant: self.tenant.clone(),
                entity_type: state.entity_type.clone(),
                entity_id: state.entity_id.clone(),
                arm_id,
                action: call.on_failure.clone(),
                due_at: due_at.to_rfc3339(),
                armed_by,
            });
        }
        rows
    }

    /// The (action, due time) of each of the record's schedules still
    /// waiting, besides `fired`, which this event removes. A call's deadline
    /// row is not one: it goes when its call answers, so a schedule matched
    /// to it would go too. A journal that cannot be read gives none, and
    /// every action is armed as asked: an arm repeated then fires once more,
    /// and lapses if its record has moved on.
    async fn waiting_schedules(
        &self,
        state: &EntityState,
        fired: Option<&str>,
    ) -> Vec<(String, DateTime<Utc>)> {
        let Some(store) = self.event_journal.as_ref() else {
            return Vec::new();
        };
        match self
            .now
            .during(
                super::watchdog::Doing::ReadingTimers,
                store.list_entity_scheduled_actions(
                    &self.tenant,
                    &state.entity_type,
                    &state.entity_id,
                ),
            )
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| {
                    Some(row.arm_id.as_str()) != fired && called_integration(&row.arm_id).is_none()
                })
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
