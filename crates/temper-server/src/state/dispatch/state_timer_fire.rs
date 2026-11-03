//! A timer's task owns its row and gate until its write is settled.
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tracing::Instrument;

use super::DispatchError;
use super::owed_writes::{OwedKind, OwedWrite};
use super::state_timeouts::ArmedTimer;
use super::timer_standing::{Standing, TimerFire};
use crate::idempotency::IdempotencyKey;
use crate::state::ServerState;
use crate::wake::TimerRef;

impl ServerState {
    /// Wait until the timer is due by the wall clock (its deadline, or
    /// sooner when a move brought it forward), fire through the
    /// normal dispatch pipeline, and remove the old row only after its
    /// durable successor is armed. A failed write keeps this arming
    /// (including its identity and journal fence) and retries. A final
    /// dispatch refusal ends the timer instead. A fire withheld for its
    /// setter's standing ends the timer and keeps its row, so whatever arms
    /// the record's timer again (a boot, an install) arms it as that setter
    /// and asks again, never as the kernel's own identity.
    pub(super) fn spawn_state_timer(
        &self,
        timer: ArmedTimer,
        gate: Option<Arc<Semaphore>>,
        armed_seq: u64,
    ) {
        let mut due_at = timer.due_at();
        let fire_key = timer.new_fire_key();
        self.state_timeout_tracker.inc_pending(&timer.entity_type);
        let workflow_root_entity_type = timer
            .agent_ctx
            .workflow_root_entity_type
            .clone()
            .unwrap_or_else(|| timer.entity_type.clone());
        let workflow_root_entity_id = timer
            .agent_ctx
            .workflow_root_entity_id
            .clone()
            .unwrap_or_else(|| timer.entity_id.clone());
        let workflow_run_id = timer
            .agent_ctx
            .workflow_run_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", timer.entity_type, timer.entity_id));

        tracing::debug!(
            tenant = %timer.tenant,
            entity_type = %timer.entity_type,
            entity_id = %timer.entity_id,
            target_state = %timer.state,
            target_action = %timer.action,
            %due_at,
            workflow.root_entity_type = %workflow_root_entity_type,
            workflow.root_entity_id = %workflow_root_entity_id,
            workflow.run_id = %workflow_run_id,
            "armed state timeout"
        );

        let due = self.due_times.arm(
            TimerRef::State {
                tenant: timer.tenant.as_str().to_string(),
                entity_type: timer.entity_type.clone(),
                entity_id: timer.entity_id.clone(),
            },
            due_at,
        );
        let state = self.clone();
        let armed_by = crate::trace_context::cause();
        tokio::spawn(async move {
            let mut kept = false;
            let mut withheld = false;
            // determinism-ok: timer work on the runtime's scheduler
            loop {
                due.wait_until(due_at).await;
                let permit = match &gate {
                    Some(gate) => gate.clone().acquire_owned().await.ok(),
                    None => None,
                };
                let span = crate::trace_context::runtime_work_span!(
                    armed_by,
                    kind = "state_timeout",
                    "dispatch.state_timeout.fire",
                    tenant = %timer.tenant,
                    entity_type = %timer.entity_type,
                    entity_id = %timer.entity_id,
                    target_state = %timer.state,
                    target_action = %timer.action,
                    workflow.root_entity_type = %workflow_root_entity_type,
                    workflow.root_entity_id = %workflow_root_entity_id,
                    workflow.run_id = %workflow_run_id,
                );
                let fired = state
                    .fire_state_timer(&timer, armed_seq, &fire_key)
                    .instrument(span)
                    .await;
                // The dispatch awaited the store answer and ran the normal
                // post-commit pipeline. No fire write survives this permit.
                drop(permit);
                let (wait, reason) = match fired {
                    Fired::Ended => break,
                    Fired::Withheld => {
                        withheld = true;
                        break;
                    }
                    Fired::Retry { wait, reason } => (wait, reason),
                };
                if !state.state_timer_is_current(&timer, armed_seq) {
                    break;
                }
                if !kept {
                    kept = state.track_owed_write(
                        fire_key.as_str(),
                        OwedWrite {
                            tenant: timer.tenant.clone(),
                            entity_type: timer.entity_type.clone(),
                            entity_id: timer.entity_id.clone(),
                            kind: OwedKind::Timer,
                            key: Some(fire_key.clone()),
                        },
                    );
                }
                tracing::warn!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                    entity_id = %timer.entity_id, arm_id = %timer.arm_id,
                    %reason, retry_in_ms = wait.as_millis() as u64,
                    "state timer retained after failed fire; retrying");
                due_at = state.due_times.retry_in(&due, wait);
            }
            if kept {
                state.untrack_owed_write(fire_key.as_str());
            }
            if withheld {
                // The row still belongs to its setter, but this task is no
                // longer live. Let an install recover it without disturbing
                // any task that has already replaced this one.
                state.state_timeout_tracker.release(&timer, armed_seq);
            } else if !state.state_timer_row_moved_on(&timer, armed_seq) {
                state.forget_state_timer(&timer).await;
            }
            state.state_timeout_tracker.dec_pending(&timer.entity_type);
            state.due_times.end(&due);
        });
    }

    /// Fire `timer` once, and say whether its arming is finished.
    async fn fire_state_timer(
        &self,
        timer: &ArmedTimer,
        armed_seq: u64,
        fire_key: &IdempotencyKey,
    ) -> Fired {
        if !self.state_timer_is_current(timer, armed_seq) {
            return Fired::Ended;
        }
        // Use the dispatch retry policy's final backoff for a store failure,
        // not a fresh full state budget or a zero-delay overdue spin.
        let policy = self.dispatch_retry_policy();
        let retry_delay =
            Duration::from_millis(policy.base_delay_ms_for(policy.max_attempts).max(1));
        let current = match self
            .get_tenant_entity_state(&timer.tenant, &timer.entity_type, &timer.entity_id)
            .await
        {
            Ok(current) => current.state,
            Err(reason) => {
                return Fired::Retry {
                    wait: retry_delay,
                    reason,
                };
            }
        };
        // Stopped because nothing names its caller (`state_timer_stop`): no
        // arming of it fires until a move names someone.
        if current.last_event.timer_needs_person.is_some() && timer.agent_ctx.caller().is_none() {
            // Cancelled, not only ended: a timer still held as armed would keep
            // the move that names someone from arming its own.
            self.cancel_state_timer(
                &timer.tenant,
                &timer.entity_type,
                &timer.entity_id,
                current.sequence_nr,
            )
            .await;
            return Fired::Ended;
        }
        let belongs = current.status == timer.state && self.declares_state_timer(timer);
        if !belongs {
            let follow = super::state_timer_reconcile::Follow {
                armed_by: Some(&timer.agent_ctx),
                woke_for: Some(timer.due_at()),
                ..Default::default()
            };
            self.follow_owned_state_timer(
                &timer.tenant,
                &timer.entity_type,
                &timer.entity_id,
                &current,
                follow,
            )
            .await;
        } else if current.sequence_nr > timer.sequence_nr {
            self.follow_moved_record(timer, armed_seq, &current).await;
        } else if !self.state_timer_already_fired(timer, &current) {
            let agent_ctx = timer.fire_context(fire_key, self.state_timeout_resets_on(timer));
            let fire = TimerFire {
                tenant: &timer.tenant,
                entity_type: &timer.entity_type,
                entity_id: &timer.entity_id,
                action: &timer.action,
            };
            match self.timer_setter_standing(&agent_ctx, &fire).await {
                Ok(Standing::Holds) => {}
                Ok(Standing::Lost { .. }) => return Fired::Withheld,
                Err(error) => {
                    return Fired::Retry {
                        wait: retry_delay,
                        reason: error.to_string(),
                    };
                }
            }
            crate::runtime_metrics::record_state_timeout_fired(
                timer.tenant.as_str(),
                &timer.entity_type,
                &timer.state,
                &timer.action,
            );
            let answer = self
                .dispatch_tenant_action_ext_typed(
                    &timer.tenant,
                    &timer.entity_type,
                    &timer.entity_id,
                    &timer.action,
                    timer.params.clone(),
                    super::DispatchExtOptions {
                        agent_ctx: &agent_ctx,
                        await_integration: false,
                        await_reactions: true,
                    },
                )
                .await;
            match answer {
                Ok(answer) if !answer.success && answer.state.sequence_nr > timer.sequence_nr => {
                    self.follow_moved_record(timer, armed_seq, &answer.state)
                        .await;
                }
                Ok(answer) if !answer.success && !answer.refused => {
                    return Fired::Retry {
                        wait: retry_delay,
                        reason: answer.error.unwrap_or_else(|| "fire write failed".into()),
                    };
                }
                Err(DispatchError::Deferred { retry_after_ms }) => {
                    return Fired::Retry {
                        wait: Duration::from_millis(retry_after_ms.max(1)),
                        reason: "dispatch deferred by admission".into(),
                    };
                }
                Err(error) if error.is_final() => {}
                Err(error) => {
                    return Fired::Retry {
                        wait: retry_delay,
                        reason: error.to_string(),
                    };
                }
                _ => {}
            }
        }
        Fired::Ended
    }
}

/// How one fire of a state timer's arming ended.
enum Fired {
    /// The arming is finished: its move was made or refused, or the record
    /// moved on.
    Ended,
    /// Its setter may no longer read the record, and its move was not made.
    /// The arming is finished, and its row is kept (`spawn_state_timer`).
    Withheld,
    /// The fire did not land; it is tried again after `wait`.
    Retry { wait: Duration, reason: String },
}
