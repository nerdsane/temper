//! A timer acts with the standing its setter has when it fires.
//!
//! A timer fires as whoever set it (`timer_cause`). Its move is the app's own,
//! the spec's consequence of the move into the timed state, so Cedar is not
//! asked about the move itself. It is asked about the setter's standing on the
//! record: whether they may read it, with the timer's cause
//! (`context.via == "timer"`). It is asked twice, with the same question.
//!
//! - When the setter's move can set a timer, before the move, as the record is
//!   then (or as it is created). The answer rides on the caller
//!   ([`AgentContext::read_when_set`]) into every timer the move sets and each
//!   timer's row.
//! - When the timer fires, as the record is now, only if the setter could read
//!   it when they set it. If they can no longer, their standing is lost and
//!   the move is not made: the fire ends as a final refusal, and it is logged,
//!   counted and kept as an authorization trajectory with the setter and
//!   Cedar's reason.
//!
//! A setter who could not read the record when they set the timer had no
//! standing on it to lose, so their timer fires as before: their move was
//! permitted without it, as a user joining an app by its link is before they
//! are its member. A timer the kernel set as one of its own identities (a
//! re-arm as the state-timer service, a module's callback, a rule's declared
//! service, an internal caller) acts for no one and fires as before
//! ([`AgentContext::caller`]). So does a timer whose row was written before
//! timers kept the answer.
//!
//! An agent's standing also rests on the token its identity was resolved
//! from. When the timer fires, the token itself is long gone, but its
//! identity kept what it was issued under
//! ([`temper_authz::SecurityContext::credential`]), and the fire asks of it
//! what the request path asks of every token: has its grant been revoked, or
//! has the user it acts for signed out everywhere since? If so, the agent has
//! been let go and its standing is lost, whether or not it could read the
//! record, and the fire ends as a lost read does. A generation that cannot be
//! read is a record that could not be read: the fire tries again, and makes
//! no move until the answer is known. A person's timer outlives their
//! sign-out, as it did before; so does the timer of an agent whose identity
//! kept no credential (one resolved some other way, or saved before
//! identities kept it).

use temper_authz::{PrincipalKind, SecurityContext};
use temper_jit::table::Effect;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::DispatchError;
use super::timer_cause::mark_timer_fire;
use crate::entity_actor::EntityActor;
use crate::identity::{IdentityError, credential_revoked};
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

/// What a timer's fire may do, by its setter's standing on the record now.
pub(super) enum Standing {
    /// The setter may read the record, could not when they set the timer, or
    /// the timer acts for no one: its move is made.
    Holds,
    /// The setter may no longer read the record, or is an agent whose token
    /// would be refused now: its move is not made.
    Lost {
        /// Cedar's reason, or why the setter's token would be refused.
        reason: String,
    },
}

/// One timer fire: the record it moves and the move it makes.
pub(super) struct TimerFire<'a> {
    pub(super) tenant: &'a TenantId,
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) action: &'a str,
}

impl ServerState {
    /// The standing of the setter of a timer whose fire is `agent` (its
    /// identity, marked with the timer's cause), on the record `fire` moves,
    /// as Cedar and the setter's credential decide now. A lost standing is
    /// reported here. An error is a record or a sign-out generation that
    /// could not be read, which the fire tries again.
    pub(super) async fn timer_setter_standing(
        &self,
        agent: &AgentContext,
        fire: &TimerFire<'_>,
    ) -> Result<Standing, DispatchError> {
        let Some(setter) = agent.caller() else {
            return Ok(Standing::Holds);
        };
        let revoked = self.agent_credential_revoked(fire.tenant, setter).await?;
        if revoked.is_none() && agent.read_when_set != Some(true) {
            return Ok(Standing::Holds);
        }
        let snapshot = self
            .load_authz_resource_snapshot(fire.tenant, fire.entity_type, fire.entity_id)
            .await?;
        let (lost, reason) = match revoked {
            Some(revoked) => ("its setter's credential would be refused now", revoked),
            None => {
                let Err(denial) = self.authorize_with_context(
                    setter,
                    crate::odata::authz::READ_ACTION,
                    fire.entity_type,
                    &snapshot.resource_attrs,
                    fire.tenant.as_str(),
                ) else {
                    return Ok(Standing::Holds);
                };
                ("its setter may not read the record now", denial.to_string())
            }
        };
        let status = snapshot.current_state.state.status;
        tracing::warn!(
            tenant = %fire.tenant,
            entity_type = fire.entity_type,
            entity_id = fire.entity_id,
            action = fire.action,
            status = %status,
            setter = %setter.principal.id,
            setter_kind = ?setter.principal.kind,
            acting_for = ?setter.principal.acting_for,
            %reason,
            "timer fire withheld: {lost}"
        );
        crate::runtime_metrics::record_timer_fire_withheld(
            fire.tenant.as_str(),
            fire.entity_type,
            fire.action,
        );
        let entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: fire.tenant.to_string(),
            entity_type: fire.entity_type.to_string(),
            entity_id: fire.entity_id.to_string(),
            action: fire.action.to_string(),
            success: false,
            from_status: Some(status),
            to_status: None,
            error: Some(format!("timer fire withheld: {lost}: {reason}")),
            agent_id: Some(setter.principal.id.clone()),
            session_id: agent.session_id.clone(),
            authz_denied: Some(true),
            denied_resource: Some(format!("{}:{}", fire.entity_type, fire.entity_id)),
            denied_module: None,
            source: Some(TrajectorySource::Authz),
            spec_governed: Some(true),
            agent_type: setter.principal.agent_type.clone(),
            request_body: Some(serde_json::to_value(&snapshot.resource_attrs).unwrap_or_default()),
            intent: agent.intent.clone(),
            matched_policy_ids: None,
            capture_seq: None,
        };
        if !self.enqueue_trajectory_entry(entry) {
            tracing::warn!("failed to enqueue withheld timer fire trajectory");
        }
        Ok(Standing::Lost { reason })
    }

    /// Why the token behind `setter`'s identity would be refused now, when
    /// `setter` is an agent whose identity kept what its token was issued
    /// under; None while it would be accepted, and for every other setter.
    /// A generation the index lists but cannot be found is refused, as the
    /// request path refuses it. An error is a generation that could not be
    /// read.
    async fn agent_credential_revoked(
        &self,
        tenant: &TenantId,
        setter: &SecurityContext,
    ) -> Result<Option<String>, DispatchError> {
        let Some(credential) = setter.credential.as_ref() else {
            return Ok(None);
        };
        if setter.principal.kind != PrincipalKind::Agent {
            return Ok(None);
        }
        match credential_revoked(self, tenant, credential).await {
            Ok(revoked) => Ok(revoked.map(|revoked| revoked.to_string())),
            Err(IdentityError::Invalid) => Ok(Some(IdentityError::Invalid.to_string())),
            Err(IdentityError::Unavailable(reason)) => Err(DispatchError::StoreUnavailable(
                format!("sign-out generation unavailable: {reason}"),
            )),
        }
    }

    /// `agent`, about to make `action` on the record, with whether they may
    /// read it as it is now when the move can set a timer
    /// ([`AgentContext::read_when_set`]), and with no answer otherwise. Every
    /// move answers afresh, so an answer about one record never reaches
    /// another's timers. Another dispatch of the move an answer was taken for
    /// (the same record, action and idempotency key) keeps it
    /// ([`AgentContext::read_when_set_for`]): a committed move recovered
    /// after its reply was lost keeps the answer taken before it, not one
    /// asked of the record its move has changed. An error is a record that
    /// could not be read.
    pub(super) async fn with_read_when_set(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        mut agent: AgentContext,
    ) -> Result<AgentContext, DispatchError> {
        let taken_for = agent.idempotency_key.as_ref().map(|key| {
            format!(
                "{tenant}:{entity_type}:{entity_id}:{action}:{}",
                key.as_str()
            )
        });
        if taken_for.is_some() && agent.read_when_set_for == taken_for {
            return Ok(agent);
        }
        agent.read_when_set = None;
        agent.read_when_set_for = None;
        if agent.caller().is_none() || !self.move_can_set_a_timer(tenant, entity_type, action) {
            return Ok(agent);
        }
        let snapshot = self
            .load_authz_resource_snapshot(tenant, entity_type, entity_id)
            .await?;
        agent.read_when_set =
            Some(self.may_read_as_timer(&agent, tenant, entity_type, &snapshot.resource_attrs));
        agent.read_when_set_for = taken_for;
        Ok(agent)
    }

    /// `created_by`, about to create the record with `initial_fields`, with
    /// whether they may read it as created when its first state has a timer
    /// ([`AgentContext::read_when_set`]), and with no answer otherwise. A
    /// creator passes what this returns to both
    /// [`Self::get_or_create_tenant_entity_by`] (the timer row its `Created`
    /// event commits) and [`Self::arm_state_timeouts_on_creation`].
    pub(crate) async fn created_with_read_when_set(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: &serde_json::Value,
        created_by: &AgentContext,
    ) -> Result<AgentContext, String> {
        let mut created_by = created_by.clone();
        created_by.read_when_set = None;
        if created_by.caller().is_none() {
            return Ok(created_by);
        }
        let (timeouts, table) = self.timers_of(tenant, entity_type);
        let Some(table) = table else {
            return Ok(created_by);
        };
        if !timeouts
            .iter()
            .any(|timeout| timeout.state == table.initial_state)
        {
            return Ok(created_by);
        }
        let created =
            EntityActor::build_initial_state(entity_type, entity_id, &table, initial_fields);
        let attrs = self
            .build_authz_resource_attrs(
                tenant,
                entity_type,
                entity_id,
                &created.status,
                &created.fields,
            )
            .await?;
        created_by.read_when_set =
            Some(self.may_read_as_timer(&created_by, tenant, entity_type, &attrs));
        Ok(created_by)
    }

    /// Whether `agent` may read a record with `attrs`, asked as a timer of
    /// theirs asks it when it fires. Counted as a check, not a refusal: a
    /// "no" here refuses nothing.
    fn may_read_as_timer(
        &self,
        agent: &AgentContext,
        tenant: &TenantId,
        entity_type: &str,
        attrs: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> bool {
        let mut asking = agent.clone();
        mark_timer_fire(&mut asking, None);
        asking.caller().is_some_and(|setter| {
            self.authorize_offer_with_context(
                setter,
                crate::odata::authz::READ_ACTION,
                entity_type,
                attrs,
                tenant.as_str(),
            )
            .is_ok()
        })
    }

    /// Whether `action` can set a timer on a record of `entity_type`: it can
    /// enter a state with a timeout, it is one of that timeout's resets, or it
    /// schedules an action.
    fn move_can_set_a_timer(&self, tenant: &TenantId, entity_type: &str, action: &str) -> bool {
        let (timeouts, table) = self.timers_of(tenant, entity_type);
        let Some(table) = table else {
            return false;
        };
        let resets = timeouts
            .iter()
            .any(|timeout| timeout.reset_on.iter().any(|reset| reset == action));
        resets
            || table
                .rules
                .iter()
                .filter(|rule| rule.name == action)
                .any(|rule| {
                    rule.effects.iter().any(|effect| {
                        matches!(
                            effect,
                            Effect::ScheduleAction { .. } | Effect::ScheduleAtAction { .. }
                        )
                    }) || rule
                        .to_state
                        .as_deref()
                        .is_some_and(|to| timeouts.iter().any(|timeout| timeout.state == to))
                })
    }

    /// `entity_type`'s state timeouts and transition table, as registered.
    fn timers_of(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> (
        Vec<temper_spec::automaton::StateTimeout>,
        Option<std::sync::Arc<temper_jit::table::TransitionTable>>,
    ) {
        let (timeouts, table) = match self.registry.read() {
            Ok(registry) => (
                registry
                    .get_spec(tenant, entity_type)
                    .map(|spec| spec.automaton.state_timeouts.clone())
                    .unwrap_or_default(),
                registry.get_table(tenant, entity_type),
            ),
            Err(_) => (Vec::new(), None),
        };
        let table = table.or_else(|| self.transition_tables.get(entity_type).cloned());
        (timeouts, table)
    }
}
