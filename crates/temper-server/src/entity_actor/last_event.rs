//! Kernel-owned provenance, captured before commit and restored with the event.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use temper_runtime::actor::ActorError;
use temper_runtime::persistence::PersistenceEnvelope;

use crate::request_context::AgentContext;
use crate::storage::BoxedEventStore;

/// The immediate source of an entity move, separate from its acting principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventVia {
    /// An authenticated request.
    Request,
    /// A state timeout or scheduled action.
    Timer,
    /// A reaction, trigger, integration callback, or other kernel-owned move.
    Reaction,
}

/// Provenance persisted with an event. Older events have unknown provenance.
///
/// `via` says how the move came about; `by` says who it ran as, however it came
/// about: a timer's move or a reaction runs as the principal that set it going,
/// and names them. A move the kernel's own identities made (`names_a_caller`)
/// names no one, so a service can never be read back as a person.
///
/// `for`, `agent_type`, `role` and `label` are recorded only with `by`, and only
/// when the subject carries them: an agent acting for a person names that
/// person, its type, its role and the name its issuer gave this instance. A
/// person acting in person records `by` and any role their token gave them.
/// They are left out of the event when absent, so events without them keep
/// their old shape.
///
/// They are what a record's journal says of who moved it, so a timer nothing
/// else names a setter for can be set as that principal
/// (`state_timer_setter`): an agent is set up again only with its role, which
/// Cedar's role forbids depend on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOrigin {
    /// How the event came about; null for events written before provenance.
    pub via: Option<EventVia>,
    /// The principal the move ran as; null when it names no caller.
    pub by: Option<String>,
    /// The principal `by` acted for, as its authenticated principal names it.
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub acting_for: Option<String>,
    /// The agent type of `by`, as its authenticated principal names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// The role of `by`, as its authenticated principal names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The name of the agent instance `by`, as its authenticated principal
    /// names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl EventOrigin {
    /// A kernel-owned move made as no one.
    pub(crate) fn internal() -> Self {
        Self {
            via: Some(EventVia::Reaction),
            ..Self::default()
        }
    }

    /// A kernel-owned move made as `caller`, such as a child its spawn makes.
    pub(crate) fn reaction_as(caller: &AgentContext) -> Self {
        Self::moved(EventVia::Reaction, caller)
    }

    /// Capture only the kernel's dispatch context, never action parameters.
    pub(crate) fn from_caller(caller: Option<&AgentContext>) -> Self {
        let Some(caller) = caller else {
            return Self::internal();
        };
        if let Some(origin) = &caller.event_origin {
            return origin.clone();
        }
        Self::moved(via_of(caller), caller)
    }

    fn moved(via: EventVia, caller: &AgentContext) -> Self {
        let Some(context) = caller.caller() else {
            return Self {
                via: Some(via),
                ..Self::default()
            };
        };
        let principal = &context.principal;
        let present = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
        Self {
            via: Some(via),
            by: Some(principal.id.clone()),
            acting_for: present(&principal.acting_for),
            agent_type: present(&principal.agent_type),
            role: present(&principal.role),
            label: present(&principal.label),
        }
    }
}

/// How the move `caller` makes comes about.
fn via_of(caller: &AgentContext) -> EventVia {
    let cause = caller
        .security_ctx
        .as_ref()
        .and_then(|ctx| ctx.cause.as_ref());
    // A reaction to a timer is a reaction itself. Cedar's `via` describes
    // the chain's root instead, so retain its existing authorization meaning.
    if cause.is_some_and(|cause| cause.reaction.is_some()) {
        return EventVia::Reaction;
    }
    if caller.fires_state_timer.is_some() || caller.fires_scheduled_action.is_some() {
        return EventVia::Timer;
    }
    if caller.callback_hops > 0 {
        return EventVia::Reaction;
    }
    if cause.is_some_and(|cause| cause.timer.is_some()) {
        return EventVia::Timer;
    }
    let Some(ctx) = caller.security_ctx.as_ref() else {
        return EventVia::Reaction;
    };
    if ctx.principal.kind == temper_authz::PrincipalKind::System
        || ctx.principal.role.as_deref() == Some("service")
    {
        return EventVia::Reaction;
    }
    EventVia::Request
}

/// Creation time and who made the record, and the newest applied event,
/// retained independently of history. Flattened into each row using the
/// kernel's top-level snake_case convention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastEvent {
    /// Timestamp at journal sequence one; unknown if that event is no longer held.
    #[serde(default, serialize_with = "serialize_first_event_at")]
    pub first_event_at: Option<DateTime<Utc>>,
    /// Immediate source of the event at journal sequence one, the record's
    /// creation. Null when that event is no longer held or predates
    /// provenance; then its maker below is unknown too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_event_via: Option<EventVia>,
    /// The principal the creation ran as, as `last_event_by` names the newest
    /// move's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_event_by: Option<String>,
    /// Whom that principal acted for, when it is an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_event_for: Option<String>,
    /// That agent's type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_event_agent_type: Option<String>,
    /// The name of that agent instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_event_label: Option<String>,
    /// Timestamp of the newest event, null when no event is known.
    pub last_event_at: Option<DateTime<Utc>>,
    /// Immediate source of that same event.
    pub last_event_via: Option<EventVia>,
    /// The principal that same event ran as, or null when it names no caller.
    pub last_event_by: Option<String>,
    /// Whom that principal acted for, when it is an agent. Saved only when
    /// present, as are its type and label: a read fills null for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_for: Option<String>,
    /// That agent's type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_agent_type: Option<String>,
    /// The name of that agent instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_label: Option<String>,
    /// The action no one could make, once a state timer stopped for it
    /// (`timer_needs_person`): a move that names someone clears it, and its
    /// timer is armed as them. Left out while there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer_needs_person: Option<String>,
}

impl LastEvent {
    /// Keep who made the record, from its creation event's provenance.
    pub(super) fn record_first(&mut self, origin: &EventOrigin) {
        self.first_event_via = origin.via;
        self.first_event_by = origin.by.clone();
        self.first_event_for = origin.acting_for.clone();
        self.first_event_agent_type = origin.agent_type.clone();
        self.first_event_label = origin.label.clone();
    }

    /// Whether the creation's provenance is known. An event from before
    /// provenance, or a creation no longer held, leaves it unknown.
    pub(super) fn first_known(&self) -> bool {
        self.first_event_via.is_some()
    }

    pub(super) fn record(&mut self, event: &super::EntityEvent) {
        self.last_event_at = Some(event.timestamp);
        self.last_event_via = event.origin.via;
        self.last_event_by = event.origin.by.clone();
        self.last_event_for = event.origin.acting_for.clone();
        self.last_event_agent_type = event.origin.agent_type.clone();
        self.last_event_label = event.origin.label.clone();
        if event.action == super::timer_needs_person::TIMER_NEEDS_PERSON_EVENT {
            self.timer_needs_person = super::timer_needs_person::refused_action(event);
        } else if event.origin.by.is_some() {
            self.timer_needs_person = None;
        }
    }
}

/// The record's creation as replay finds it: when and who made it, each kept
/// from the snapshot when known, else read from the event at sequence one, the
/// replayed tail's first or, after a snapshot, through the journal index; never
/// the oldest retained event. An event from before provenance names no one.
pub(super) async fn creation(
    store: &BoxedEventStore,
    persistence_id: &str,
    known: &LastEvent,
    envelopes: &[PersistenceEnvelope],
    loaded_snapshot: bool,
) -> Result<(Option<DateTime<Utc>>, Option<EventOrigin>), ActorError> {
    let lookup = |what: &str, error: &dyn std::fmt::Display| {
        ActorError::custom(format!("creation {what} lookup failed: {error}"))
    };
    let origin_of = |event: &PersistenceEnvelope| {
        EventOrigin::deserialize(&event.payload)
            .ok()
            .filter(|origin| origin.via.is_some())
    };
    let first = envelopes.first().filter(|event| event.sequence_nr == 1);
    let at = if known.first_event_at.is_some() {
        known.first_event_at
    } else if let Some(event) = first {
        Some(
            event
                .payload
                .get("timestamp")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or(event.metadata.timestamp),
        )
    } else if loaded_snapshot {
        store
            .first_event_at(persistence_id)
            .await
            .map_err(|error| lookup("time", &error))?
    } else {
        None
    };
    let origin = if known.first_known() {
        None
    } else if let Some(event) = first {
        origin_of(event)
    } else if loaded_snapshot {
        store
            .read_events_page(persistence_id, Some(2), 1)
            .await
            .map_err(|error| lookup("maker", &error))?
            .first()
            .filter(|event| event.sequence_nr == 1)
            .and_then(origin_of)
    } else {
        None
    };
    Ok((at, origin))
}

fn serialize_first_event_at<S: serde::Serializer>(
    timestamp: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match timestamp {
        Some(timestamp) => serializer
            .serialize_some(&timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use temper_authz::{MoveCause, PrincipalKind, ReactionHop, SecurityContext, TimerArm};

    /// A person in person, as a verified token with only a `sub` resolves.
    fn caller() -> AgentContext {
        AgentContext {
            security_ctx: Some(SecurityContext::from_verified_jwt(
                "ann",
                PrincipalKind::Customer,
                None,
                None,
                Some("owner"),
                None,
            )),
            // An observation field is not the authenticated subject.
            agent_id: Some("forged-observation".into()),
            ..AgentContext::default()
        }
    }

    /// An agent acting for ann, as a verified token with an `agent_type` resolves.
    fn agent() -> AgentContext {
        AgentContext {
            security_ctx: Some(SecurityContext::from_verified_jwt(
                "hub-client",
                PrincipalKind::Agent,
                Some("claude-code"),
                Some("ann"),
                Some("contributor"),
                None,
            )),
            // Observation fields are not the authenticated subject or its type.
            agent_id: Some("forged-observation".into()),
            agent_type: Some("forged-type".into()),
            ..AgentContext::default()
        }
    }

    fn timer_cause() -> MoveCause {
        MoveCause::timer_fire(TimerArm {
            armed_by: "ann".into(),
            state_timer_of: None,
        })
    }

    fn reaction_cause() -> MoveCause {
        MoveCause::reaction(
            ReactionHop {
                rule: "Job:Expire:finish".into(),
                action: "Expire".into(),
                entity_type: "Job".into(),
                entity_id: "job".into(),
            },
            Some(&timer_cause()),
        )
    }

    /// Ann in person, moving as `via`.
    fn ann(via: EventVia) -> EventOrigin {
        EventOrigin {
            via: Some(via),
            by: Some("ann".into()),
            role: Some("owner".into()),
            ..EventOrigin::default()
        }
    }

    /// The agent acting for ann, moving as `via`.
    fn anns_agent(via: EventVia) -> EventOrigin {
        EventOrigin {
            via: Some(via),
            by: Some("hub-client".into()),
            acting_for: Some("ann".into()),
            agent_type: Some("claude-code".into()),
            role: Some("contributor".into()),
            label: None,
        }
    }

    #[test]
    fn every_move_names_who_it_ran_as_and_says_how_it_came_about() {
        let mut caller = caller();
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            ann(EventVia::Request)
        );
        caller.security_ctx.as_mut().unwrap().cause = Some(timer_cause());
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            ann(EventVia::Timer)
        );
        caller.security_ctx.as_mut().unwrap().cause = Some(reaction_cause());
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            ann(EventVia::Reaction)
        );
    }

    #[test]
    fn a_kernel_identitys_timer_and_reaction_moves_name_no_one() {
        for mut kernel in [
            AgentContext::for_service("state-timer"),
            AgentContext::system(),
        ] {
            kernel.security_ctx.as_mut().unwrap().cause = Some(timer_cause());
            assert_eq!(
                EventOrigin::from_caller(Some(&kernel)),
                EventOrigin {
                    via: Some(EventVia::Timer),
                    ..EventOrigin::default()
                }
            );
            kernel.security_ctx.as_mut().unwrap().cause = Some(reaction_cause());
            assert_eq!(
                EventOrigin::from_caller(Some(&kernel)),
                EventOrigin::internal()
            );
        }
    }

    #[test]
    fn an_agent_request_records_whom_it_acted_for_and_its_type() {
        let origin = EventOrigin::from_caller(Some(&agent()));
        assert_eq!(
            origin,
            EventOrigin {
                via: Some(EventVia::Request),
                by: Some("hub-client".into()),
                acting_for: Some("ann".into()),
                agent_type: Some("claude-code".into()),
                role: Some("contributor".into()),
                label: None,
            }
        );
        // The journaled event carries them at its top level, as `for`,
        // `agent_type` and `role`, and reads them back.
        let mut event = super::super::EntityEvent::bootstrap("Idle".into());
        event.origin = origin;
        let journaled = serde_json::to_value(&event).unwrap();
        assert_eq!(journaled["by"], "hub-client");
        assert_eq!(journaled["for"], "ann");
        assert_eq!(journaled["agent_type"], "claude-code");
        assert_eq!(journaled["role"], "contributor");
        assert!(journaled.get("acting_for").is_none(), "{journaled}");
        let read: super::super::EntityEvent = serde_json::from_value(journaled).unwrap();
        assert_eq!(read.origin, event.origin);
    }

    #[test]
    fn a_person_in_person_records_only_by_and_role() {
        let origin = EventOrigin::from_caller(Some(&caller()));
        assert_eq!(
            origin,
            EventOrigin {
                via: Some(EventVia::Request),
                by: Some("ann".into()),
                acting_for: None,
                agent_type: None,
                role: Some("owner".into()),
                label: None,
            }
        );
        let mut event = super::super::EntityEvent::bootstrap("Idle".into());
        event.origin = origin;
        let journaled = serde_json::to_value(&event).unwrap();
        assert!(journaled.get("for").is_none(), "{journaled}");
        assert!(journaled.get("agent_type").is_none(), "{journaled}");
    }

    #[test]
    fn timer_and_reaction_moves_keep_the_agent_they_ran_as() {
        let mut fired = agent();
        fired.security_ctx.as_mut().unwrap().cause = Some(timer_cause());
        assert_eq!(
            EventOrigin::from_caller(Some(&fired)),
            anns_agent(EventVia::Timer)
        );
        let mut scheduled = agent();
        scheduled.fires_scheduled_action = Some(crate::request_context::ScheduledFire {
            tenant: "default".into(),
            entity_type: "Job".into(),
            entity_id: "job".into(),
            arm_id: "arm".into(),
        });
        assert_eq!(
            EventOrigin::from_caller(Some(&scheduled)),
            anns_agent(EventVia::Timer)
        );
        let mut reaction = agent();
        reaction.security_ctx.as_mut().unwrap().cause = Some(reaction_cause());
        assert_eq!(
            EventOrigin::from_caller(Some(&reaction)),
            anns_agent(EventVia::Reaction)
        );
        let mut callback = agent();
        callback.callback_hops = 1;
        assert_eq!(
            EventOrigin::from_caller(Some(&callback)),
            anns_agent(EventVia::Reaction)
        );
    }

    #[test]
    fn callbacks_are_reactions_even_when_their_caller_is_a_request() {
        let mut caller = caller();
        caller.callback_hops = 1;
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            ann(EventVia::Reaction)
        );
        assert_eq!(EventOrigin::from_caller(None), EventOrigin::internal());
        assert_eq!(
            EventOrigin::from_caller(Some(&AgentContext::system())),
            EventOrigin::internal()
        );
        assert_eq!(
            EventOrigin::from_caller(Some(&AgentContext::for_service("test"))),
            EventOrigin::internal()
        );
    }

    #[test]
    fn a_scheduled_fire_inside_a_callback_is_a_timer_and_its_answer_is_a_reaction() {
        let mut caller = caller();
        caller.callback_hops = 3; // Zero-delay schedules retain their callback budget.
        caller.security_ctx.as_mut().unwrap().cause = Some(timer_cause());
        caller.fires_scheduled_action = Some(crate::request_context::ScheduledFire {
            tenant: "default".into(),
            entity_type: "Job".into(),
            entity_id: "job".into(),
            arm_id: "arm".into(),
        });
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            ann(EventVia::Timer)
        );
        let answer = caller
            .for_callback("Job", "job", "Answer", "test")
            .unwrap()
            .for_answer();
        assert_eq!(
            EventOrigin::from_caller(Some(&answer)),
            ann(EventVia::Reaction)
        );
    }

    #[test]
    fn a_move_that_names_someone_clears_what_a_stopped_timer_says() {
        let event = |action: &str, origin: EventOrigin, params: serde_json::Value| {
            let mut event = super::super::EntityEvent::bootstrap("Idle".into());
            event.action = action.into();
            event.origin = origin;
            event.params = params;
            event
        };
        let mut last = LastEvent::default();
        last.record(&event(
            super::super::timer_needs_person::TIMER_NEEDS_PERSON_EVENT,
            EventOrigin::internal(),
            serde_json::json!({"action": "Diagnose"}),
        ));
        assert_eq!(last.timer_needs_person.as_deref(), Some("Diagnose"));
        last.record(&event("Poll", EventOrigin::internal(), Value::Null));
        assert_eq!(
            last.timer_needs_person.as_deref(),
            Some("Diagnose"),
            "a move that names no one leaves it"
        );
        last.record(&event("Nudge", ann(EventVia::Request), Value::Null));
        assert_eq!(last.timer_needs_person, None);
    }

    #[test]
    fn the_record_keeps_who_made_it_while_later_moves_name_the_newest() {
        let mut state = super::super::EntityState {
            entity_type: "Note".into(),
            entity_id: "n1".into(),
            status: "Idle".into(),
            item_count: 0,
            counters: Default::default(),
            booleans: Default::default(),
            lists: Default::default(),
            fields: serde_json::json!({}),
            events: Default::default(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: Default::default(),
            last_event: Default::default(),
            status_clock: Default::default(),
            spawn_reservations: Default::default(),
        };
        let mut created = super::super::EntityEvent::bootstrap("Idle".into());
        created.action = "Create".into();
        created.origin = anns_agent(EventVia::Request);
        state.push_event_bounded(created);
        let mut moved = super::super::EntityEvent::bootstrap("Idle".into());
        moved.action = "Edit".into();
        moved.origin = EventOrigin {
            via: Some(EventVia::Request),
            by: Some("bo".into()),
            ..EventOrigin::default()
        };
        state.push_event_bounded(moved);
        let last = &state.last_event;
        assert_eq!(last.first_event_via, Some(EventVia::Request));
        assert_eq!(last.first_event_by.as_deref(), Some("hub-client"));
        assert_eq!(last.first_event_for.as_deref(), Some("ann"));
        assert_eq!(last.first_event_agent_type.as_deref(), Some("claude-code"));
        assert_eq!(last.last_event_by.as_deref(), Some("bo"));
        assert_eq!(last.last_event_for, None);
        // The row carries them at its top level and reads them back.
        let row = serde_json::to_value(last).unwrap();
        assert_eq!(row["first_event_by"], "hub-client");
        assert_eq!(row["first_event_for"], "ann");
        assert_eq!(row["first_event_via"], "request");
        let read: LastEvent = serde_json::from_value(row).unwrap();
        assert_eq!(&read, last);
    }

    #[test]
    fn a_row_saved_before_makers_were_kept_reads_as_unknown() {
        let read: LastEvent = serde_json::from_value(serde_json::json!({
            "first_event_at": "2026-01-01T00:00:00Z", "last_event_by": "bo"
        }))
        .unwrap();
        assert!(!read.first_known());
        assert_eq!(read.first_event_by, None);
        let row = serde_json::to_value(&read).unwrap();
        assert!(row.get("first_event_by").is_none(), "{row}");
    }

    #[test]
    fn old_events_do_not_invent_provenance() {
        let event: super::super::EntityEvent = serde_json::from_value(serde_json::json!({
            "action":"Expire", "from_status":"Waiting", "to_status":"Done",
            "timestamp":"2026-01-01T00:03:00Z", "params":{"moved_by":"stale"}
        }))
        .unwrap();
        assert_eq!(event.origin, EventOrigin::default());
        let mut last = LastEvent::default();
        last.record(&event);
        assert_eq!(last.last_event_at, Some(event.timestamp));
        assert_eq!(last.last_event_via, None);
        assert_eq!(last.last_event_by, None);
    }
}
