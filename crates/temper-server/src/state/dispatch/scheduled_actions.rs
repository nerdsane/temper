//! Scheduled actions (`schedule` and `schedule_at`) as durable timers.
//!
//! The actor writes a scheduled action's row in the commit of the event that
//! armed it (`EntityActor::arm_scheduled_actions`), so the rows are the
//! scheduled actions still waiting. After the commit, and after the
//! transition's integrations, this process arms each one
//! ([`ServerState::arm_scheduled_actions`]) on every path where the event was
//! written, and at once when the transition's projection write fails (then
//! only there); at boot [`ServerState::rearm_scheduled_actions`] arms every
//! row again, through the boot gate, for the rest of its wait or at once if it
//! fell due while the server was down. Replay never arms one: the rows are the
//! source of truth. While a row's timer is live, arming it again does nothing;
//! once it has fired, a second arm fires it again, so each transition arms its
//! rows once.
//!
//! A fire runs as whoever armed it. The event it writes removes its row in the
//! same commit, so a row is removed only once its action has committed, and a
//! restart cannot fire it twice. A fire whose write failed or may have landed
//! keeps its row and is retried on it, with the same idempotency key derived
//! from the parent request (or the arm id for an unkeyed parent) and the waits
//! of an owed write (`owed_writes`): a write that did
//! land is a duplicate and does not run again. A refused fire is final: it is
//! logged here, its dispatch records it in the record's trajectory like any
//! refused action, and then its row is removed. So is a fire whose dispatch
//! failed with an error no retry can change ([`DispatchError::is_final`]),
//! such as a caller who cannot make the move: every try would be answered
//! the same, so it is logged once and its row removed, not retried.
//!
//! A fire whose action has no transition from the record's status when it
//! comes due has lapsed ([`scheduled_fire_lapsed`]): the record left the
//! states it was armed for, as a timer whose state was left does not fire.
//! It is not a refusal anyone acts on, so it writes no trajectory entry; it
//! is counted, logged at debug and its row removed. A guard that does not
//! hold is still a refusal, recorded as one.

mod recovery;

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use temper_runtime::persistence::ScheduledActionRecord;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;
use tracing::Instrument;

use super::effects::PostDispatchContext;
use super::owed_writes::{FIRST_RETRY_WAIT, LAST_RETRY_WAIT};
use super::state_timer_follow::BootTimers;
use super::timer_standing::{Standing, TimerFire};
use super::{DispatchError, DispatchExtOptions};
use crate::entity_actor::EntityResponse;
use crate::request_context::armed_by::decode_armed_by;
use crate::request_context::{AgentContext, ScheduledFire};
use crate::state::ServerState;
use crate::wake::{Armed, TimerRef};

/// A scheduled action armed in this process.
struct ScheduledTimer {
    tenant: TenantId,
    entity_type: String,
    entity_id: String,
    /// The row's id: the arming event's sequence and the action's index.
    arm_id: String,
    action: String,
    /// When it is due, as its row says.
    due_at: DateTime<Utc>,
    /// Who armed it; it fires as them.
    agent_ctx: AgentContext,
}

impl ScheduledTimer {
    /// The row this timer fires.
    fn row(&self) -> ScheduledFire {
        ScheduledFire {
            tenant: self.tenant.as_str().to_string(),
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            arm_id: self.arm_id.clone(),
        }
    }
}

impl ServerState {
    /// The scheduled actions armed in this process: each row's live timers.
    pub fn armed_scheduled_actions(&self) -> BTreeMap<ScheduledFire, usize> {
        self.scheduled_armed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Arm the scheduled actions a committed transition armed. Their rows
    /// were written with its event.
    pub(super) fn arm_scheduled_actions(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        for (index, scheduled) in response.scheduled_actions.iter().enumerate() {
            // A record with no journal keeps no row: its action is due its
            // delay from now. One past the end of the calendar never is.
            let due_at = scheduled.due_at.unwrap_or_else(|| {
                i64::try_from(scheduled.delay_millis)
                    .ok()
                    .and_then(chrono::Duration::try_milliseconds)
                    .and_then(|delay| sim_now().checked_add_signed(delay))
                    .unwrap_or(DateTime::<Utc>::MAX_UTC)
            });
            let timer = ScheduledTimer {
                tenant: ctx.tenant.clone(),
                entity_type: ctx.entity_type.to_string(),
                entity_id: ctx.entity_id.to_string(),
                arm_id: scheduled.arm_id.clone(),
                action: scheduled.action.clone(),
                due_at,
                agent_ctx: ctx.agent_ctx.for_scheduled_follow_on(
                    &scheduled.action,
                    index,
                    scheduled.delay_millis,
                ),
            };
            self.spawn_scheduled_action(timer, None);
        }
    }

    /// Arm again every scheduled action a previous process left waiting
    /// (`rearm_state_timers` calls it at boot), each through `gate`, and
    /// count them per tenant and type into `counts`. A row that cannot be
    /// read is removed: it can never fire. A tenant whose rows cannot be
    /// listed is tried again in the background until they can.
    #[tracing::instrument(skip_all, fields(otel.name = "boot.scheduled_actions"))]
    pub(super) async fn rearm_scheduled_actions(
        &self,
        gate: &Arc<Semaphore>,
        counts: &mut BootTimers,
    ) {
        let tenants = match self.registry.read() {
            Ok(registry) => registry
                .tenant_ids()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            Err(_) => return,
        };
        for tenant in tenants {
            if let Err(error) = self.rearm_tenant_rows(&tenant, gate, counts).await {
                tracing::error!(tenant = %tenant, %error,
                    retry_in_ms = FIRST_RETRY_WAIT.as_millis() as u64,
                    "scheduled action rows not readable at boot; retrying");
                self.retry_tenant_rows(tenant, gate.clone());
            }
        }
    }

    /// List `tenant`'s rows again until they can be read, and arm them.
    fn retry_tenant_rows(&self, tenant: TenantId, gate: Arc<Semaphore>) {
        let server = self.clone();
        let retry = async move {
            let mut wait = FIRST_RETRY_WAIT;
            loop {
                tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                let mut counts = BootTimers::default();
                match server.rearm_tenant_rows(&tenant, &gate, &mut counts).await {
                    Ok(()) => return,
                    Err(error) => {
                        wait = (wait * 2).min(LAST_RETRY_WAIT);
                        tracing::error!(tenant = %tenant, %error,
                            retry_in_ms = wait.as_millis() as u64,
                            "scheduled action rows still not readable; retrying");
                    }
                }
            }
        };
        tokio::spawn(retry); // determinism-ok: boot recovery retry is a background side-effect
    }

    /// Arm every row of `tenant` this process has not armed, through `gate`.
    async fn rearm_tenant_rows(
        &self,
        tenant: &TenantId,
        gate: &Arc<Semaphore>,
        counts: &mut BootTimers,
    ) -> Result<(), String> {
        let Some((store, _)) = self.event_journal() else {
            return Ok(());
        };
        let rows = store
            .list_scheduled_actions(tenant.as_str())
            .await
            .map_err(|error| error.to_string())?;
        for row in rows {
            let timer = match scheduled_timer(tenant, &row) {
                Ok(armed) => armed,
                Err(reason) => {
                    tracing::warn!(tenant = %tenant, entity_type = %row.entity_type,
                        entity_id = %row.entity_id, action = %row.action, %reason,
                        "scheduled action row dropped at boot");
                    self.forget_scheduled_action(
                        tenant,
                        &row.entity_type,
                        &row.entity_id,
                        &row.arm_id,
                    )
                    .await;
                    continue;
                }
            };
            counts
                .entry((tenant.clone(), row.entity_type.clone()))
                .or_default()
                .scheduled += 1;
            self.spawn_scheduled_action(timer, Some(gate.clone()));
        }
        Ok(())
    }

    /// Arm `timer` to fire when it is due by the wall clock, unless this
    /// process has armed its row already.
    fn spawn_scheduled_action(&self, timer: ScheduledTimer, gate: Option<Arc<Semaphore>>) {
        let row = timer.row();
        {
            let mut armed = self
                .scheduled_armed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let live = armed.entry(row.clone()).or_insert(0);
            if *live > 0 {
                return;
            }
            *live += 1;
        }
        let due = self
            .due_times
            .arm(TimerRef::Scheduled(row.clone()), timer.due_at);
        // The fire's own trace starts when it fires, linked to the move that
        // armed it (none for a row armed at boot).
        let armed_by = crate::trace_context::cause();
        let server = self.clone();
        let fire = async move {
            due.wait_until(timer.due_at).await;
            let agent = &timer.agent_ctx;
            let span = crate::trace_context::runtime_work_span!(
                armed_by,
                "dispatch.scheduled_actions",
                workflow.root_entity_type = %agent.workflow_root_entity_type.as_deref().unwrap_or(&timer.entity_type),
                workflow.root_entity_id = %agent.workflow_root_entity_id.as_deref().unwrap_or(&timer.entity_id),
                workflow.run_id = %agent.workflow_run_id.clone()
                    .unwrap_or_else(|| format!("{}:{}", timer.entity_type, timer.entity_id)),
                temper.action = %timer.action,
                entity_type = %timer.entity_type,
                entity_id = %timer.entity_id,
            );
            server
                .fire_scheduled_action(&timer, gate, &due)
                .instrument(span)
                .await;
            server.due_times.end(&due);
            let mut armed = server
                .scheduled_armed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(live) = armed.get_mut(&row) {
                *live -= 1;
                if *live == 0 {
                    armed.remove(&row);
                }
            }
        };
        tokio::spawn(fire); // determinism-ok: timer delivery is a background side-effect
    }

    /// Fire `timer` until its action commits, is refused or fails with an
    /// error no retry can change, each attempt through `gate` when one is
    /// given. A refusal or such an error is logged and its row removed; a
    /// commit removed the row itself. A fire whose setter may no longer read
    /// the record is refused the same way (`timer_standing`): nothing arms a
    /// scheduled action again without its row. Between tries it is due again
    /// when it next tries, by the wall clock.
    async fn fire_scheduled_action(
        &self,
        timer: &ScheduledTimer,
        gate: Option<Arc<Semaphore>>,
        due: &Armed,
    ) {
        let key = timer.agent_ctx.idempotency_key.clone().unwrap_or_else(|| {
            crate::idempotency::IdempotencyKey::Request(format!("schedule:{}", timer.arm_id))
        });
        let mut agent = timer.agent_ctx.clone();
        // The same action every attempt, so a retry of a fire that landed is
        // answered as it.
        agent.idempotency_key = Some(key.clone());
        agent.event_origin = None;
        agent.fires_scheduled_action = Some(timer.row());
        super::timer_cause::mark_timer_fire(&mut agent, None);
        let mut wait = FIRST_RETRY_WAIT;
        let mut retried = false;
        // The last attempt's caller, with the answer the timers its move sets
        // keep (`timer_standing`), taken before that attempt.
        let mut answered: Option<AgentContext> = None;
        loop {
            // Recover a move that already committed before asking whether
            // another attempt may run. Its successors are owed even if that
            // move itself changed the standing that allowed its setter to read.
            let committed = retried && self.arm_fired_rows_retrying(timer, key.as_str()).await;
            // A committed fire still needs the normal post-dispatch pipeline:
            // its state timer, status cache and query projection may be owed.
            // Dispatch with the same key recovers the actor's duplicate reply
            // and completes those effects, without authorizing another move.
            // Its timers keep the answer of the attempt that committed it,
            // the last one: the record it would be asked of now is the one
            // its move made.
            let outcome = {
                let _permit = match &gate {
                    Some(gate) => gate.clone().acquire_owned().await.ok(),
                    None => None,
                };
                let fire = TimerFire {
                    tenant: &timer.tenant,
                    entity_type: &timer.entity_type,
                    entity_id: &timer.entity_id,
                    action: &timer.action,
                };
                let standing = if committed {
                    Ok(Standing::Holds)
                } else {
                    self.timer_setter_standing(&agent, &fire).await
                };
                let caller = match (standing, answered.take()) {
                    (Ok(Standing::Holds), Some(caller)) if committed => Ok(caller),
                    (Ok(Standing::Holds), _) => {
                        self.with_read_when_set(
                            &timer.tenant,
                            &timer.entity_type,
                            &timer.entity_id,
                            &timer.action,
                            agent.clone(),
                        )
                        .await
                    }
                    (Ok(Standing::Lost { reason }), _) => Err(DispatchError::AuthzDenied(reason)),
                    (Err(error), _) => Err(error),
                };
                match caller {
                    Ok(caller) => {
                        let options = DispatchExtOptions {
                            agent_ctx: &caller,
                            await_integration: false,
                            await_reactions: true,
                        };
                        let outcome = self
                            .dispatch_tenant_action_ext_typed(
                                &timer.tenant,
                                &timer.entity_type,
                                &timer.entity_id,
                                &timer.action,
                                serde_json::json!({}),
                                options,
                            )
                            .await;
                        answered = Some(caller);
                        outcome
                    }
                    Err(error) => Err(error),
                }
            };
            let (error, is_final) = match outcome {
                Ok(response) if scheduled_fire_lapsed(Some(&timer.row()), &response) => {
                    crate::runtime_metrics::record_scheduled_fire_lapsed(
                        timer.tenant.as_str(),
                        &timer.entity_type,
                        &timer.action,
                    );
                    tracing::debug!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                        entity_id = %timer.entity_id, action = %timer.action,
                        arm_id = %timer.arm_id, status = %response.state.status,
                        "scheduled action came due where its action has no transition; \
                         nothing fired and its row is removed");
                    self.forget_scheduled_action(
                        &timer.tenant,
                        &timer.entity_type,
                        &timer.entity_id,
                        &timer.arm_id,
                    )
                    .await;
                    return;
                }
                Ok(response) if response.success => {
                    if retried {
                        self.arm_fired_rows_retrying(timer, key.as_str()).await;
                    }
                    return;
                }
                Ok(response) if response.refused => (
                    response
                        .error
                        .unwrap_or_else(|| "guard not met".to_string()),
                    true,
                ),
                Ok(response) => (
                    response.error.unwrap_or_else(|| "write failed".to_string()),
                    false,
                ),
                Err(error) => (error.to_string(), DispatchError::is_final(&error)),
            };
            if is_final {
                tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                    entity_id = %timer.entity_id, action = %timer.action,
                    arm_id = %timer.arm_id, %error,
                    "scheduled action refused when it fired; its row is removed");
                self.forget_scheduled_action(
                    &timer.tenant,
                    &timer.entity_type,
                    &timer.entity_id,
                    &timer.arm_id,
                )
                .await;
                return;
            }
            tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                entity_id = %timer.entity_id, action = %timer.action, arm_id = %timer.arm_id,
                %error, retry_in_ms = wait.as_millis() as u64,
                "scheduled action did not commit when it fired; retrying on its row");
            let again = self.due_times.retry_in(due, wait);
            due.wait_until(again).await;
            wait = (wait * 2).min(LAST_RETRY_WAIT);
            retried = true;
        }
    }

    /// Remove a refused scheduled action's row, or one that can never fire or
    /// never land. A row left behind fires again at the next boot.
    async fn forget_scheduled_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) {
        let Some((store, _)) = self.event_journal() else {
            return;
        };
        if let Err(error) = store
            .delete_scheduled_action(tenant.as_str(), entity_type, entity_id, arm_id)
            .await
        {
            tracing::warn!(tenant = %tenant, entity_type, entity_id, arm_id, %error,
                "fired scheduled action row not removed; the next boot finds it a duplicate");
        }
    }
}

/// Whether `response` answers the fire of `fire`, a scheduled action of the
/// answered record, refused only because no transition for its action starts
/// from the record's status: the fire lapsed, and nothing is recorded for it.
/// A refused guard is not a lapse; it is recorded as the refusal it is.
pub(crate) fn scheduled_fire_lapsed(
    fire: Option<&ScheduledFire>,
    response: &EntityResponse,
) -> bool {
    fire.is_some_and(|fire| {
        fire.entity_type == response.state.entity_type && fire.entity_id == response.state.entity_id
    }) && response.refused
        && matches!(
            response.refusal,
            Some(temper_jit::table::Refusal::InvalidState { .. })
        )
}

/// The timer a row arms.
fn scheduled_timer(
    tenant: &TenantId,
    row: &ScheduledActionRecord,
) -> Result<ScheduledTimer, String> {
    let due_at = DateTime::parse_from_rfc3339(&row.due_at)
        .map_err(|error| format!("due_at: {error}"))?
        .with_timezone(&Utc);
    let agent_ctx = decode_armed_by(&row.armed_by).map_err(|error| format!("armed_by: {error}"))?;
    let timer = ScheduledTimer {
        tenant: tenant.clone(),
        entity_type: row.entity_type.clone(),
        entity_id: row.entity_id.clone(),
        arm_id: row.arm_id.clone(),
        action: row.action.clone(),
        due_at,
        agent_ctx,
    };
    Ok(timer)
}
