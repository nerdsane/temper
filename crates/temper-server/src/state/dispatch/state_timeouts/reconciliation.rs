//! Actor hydration and synthetic-commit timeout reconciliation.

use tokio::spawn as spawn_timeout_hydration; // determinism-ok: one bounded task per actor startup lifecycle

use crate::entity_actor::recover_entity_state_from_store;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StateTimeoutHydrationTiming {
    pub(super) observed_at: DateTime<Utc>,
    pub(super) readiness_elapsed: Duration,
}

impl crate::state::ServerState {
    /// Reconcile a newly spawned actor's durable state with its declared timeout.
    ///
    /// The state read is synchronously admitted as the new actor's first
    /// mailbox message before its [`temper_runtime::actor::ActorRef`] is
    /// published. This task awaits that lifecycle-coupled reply, so neither
    /// slow startup nor already-queued application traffic can overtake or
    /// exhaust reconciliation. Keeping this hook on `ServerState` avoids a
    /// strong `ServerState -> actor -> ServerState` cycle.
    pub(crate) fn schedule_state_timeout_hydration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        actor_uid: uuid::Uuid,
        startup_state: temper_runtime::actor::PendingAsk<EntityResponse>,
        hydration_completion: tokio::sync::watch::Sender<bool>,
    ) {
        let state = self.clone();
        let tenant = tenant.clone();
        let entity_type = entity_type.to_string();
        let entity_id = entity_id.to_string();
        let observed_at = sim_now();
        let readiness_started_at = tokio::time::Instant::now(); // determinism-ok: paused by DST

        spawn_timeout_hydration(async move {
            match startup_state.receive().await {
                Ok(response) => {
                    if response.state.status == "Deleted" {
                        state
                            .retire_deleted_hydration_if_current(
                                &tenant,
                                &entity_type,
                                &entity_id,
                                actor_uid,
                                &response,
                            )
                            .await;
                    } else {
                        state.arm_state_timeouts_on_current_actor_hydration(
                            &tenant,
                            &entity_type,
                            &entity_id,
                            actor_uid,
                            &response,
                            StateTimeoutHydrationTiming {
                                observed_at,
                                readiness_elapsed: readiness_started_at.elapsed(),
                            },
                        );
                    }
                }
                Err(error) => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        error = %error,
                        "state timeout hydration actor stopped before startup reconciliation"
                    );
                }
            }
            state.state_timeout_tracker.complete_hydration(
                &tenant,
                &entity_type,
                &entity_id,
                actor_uid,
                hydration_completion,
            );
        });
    }

    /// Retire a startup snapshot that authoritatively hydrated as `Deleted`.
    ///
    /// Registry validation is held through fence creation so a delayed
    /// hydration callback cannot leave a new inactive owner after another
    /// caller already evicted this UID.
    pub(crate) async fn retire_deleted_hydration_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        actor_uid: uuid::Uuid,
        response: &EntityResponse,
    ) {
        debug_assert_eq!(response.state.status, "Deleted");
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
        let inactive_timeout_fence = {
            let registry = match self.actor_registry.read() {
                Ok(registry) => registry,
                Err(_) => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        actor_uid = %actor_uid,
                        "Deleted hydration skipped because the actor registry lock is poisoned"
                    );
                    return;
                }
            };
            let is_current = registry
                .get(&actor_key)
                .is_some_and(|actor_ref| actor_ref.id().uid == actor_uid);
            if !is_current {
                return;
            }
            self.reconcile_state_timeout_after_synthetic_commit(
                tenant,
                entity_type,
                entity_id,
                &response.state,
            )
        };

        if self
            .stop_and_remove_entity_if_current(tenant, entity_type, entity_id, actor_uid)
            .await
        {
            self.release_inactive_state_timeout_after_actor_eviction(
                tenant,
                entity_type,
                entity_id,
                inactive_timeout_fence,
            );
        }
    }

    /// Recover a benign timeout cancellation when readiness discovers a
    /// durable tombstone and retires the hydrated actor before dispatch.
    ///
    /// Ordinary readers treat that readiness outcome as absence. A timeout
    /// already admitted before the terminal commit must instead observe the
    /// same actor-atomic precondition mismatch it would have received had its
    /// message reached the now-retired actor.
    pub(crate) async fn recover_deleted_state_timeout_cancellation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<EntityResponse>, String> {
        let Some((store, backend)) = self.event_journal() else {
            return Ok(None);
        };
        let table = self
            .registry
            .read()
            .map_err(|_| {
                "registry lock poisoned while recovering timeout cancellation".to_string()
            })?
            .get_table(tenant, entity_type)
            .or_else(|| self.transition_tables.get(entity_type).cloned());
        let Some(table) = table else {
            return Ok(None);
        };
        let blob_store = self.blob_store_for_tenant(tenant).ok();
        let state = recover_entity_state_from_store(
            tenant.as_str(),
            entity_type,
            entity_id,
            &table,
            &store,
            backend,
            &serde_json::json!({}),
            blob_store.as_ref(),
            true,
        )
        .await
        .map_err(|error| {
            format!("failed to recover timeout cancellation for {entity_type}:{entity_id}: {error}")
        })?;
        if state.status != "Deleted" {
            return Ok(None);
        }
        Ok(Some(EntityResponse {
            success: false,
            state,
            error: Some(STATE_TIMEOUT_PRECONDITION_MISMATCH.to_string()),
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        }))
    }

    pub(super) fn arm_state_timeouts_on_current_actor_hydration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        actor_uid: uuid::Uuid,
        response: &EntityResponse,
        timing: StateTimeoutHydrationTiming,
    ) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
        let registry = match self.actor_registry.read() {
            Ok(registry) => registry,
            Err(_) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    "state timeout hydration skipped because the actor registry lock is poisoned"
                );
                return;
            }
        };
        let is_current = registry
            .get(&actor_key)
            .is_some_and(|actor_ref| !actor_ref.is_stopped() && actor_ref.id().uid == actor_uid);
        if !is_current {
            tracing::debug!(
                tenant = %tenant,
                entity_type,
                entity_id,
                actor_uid = %actor_uid,
                "discarding state timeout hydration from an evicted actor incarnation"
            );
            return;
        }

        self.arm_state_timeouts_on_hydration(
            tenant,
            entity_type,
            entity_id,
            response,
            timing.observed_at,
            timing.readiness_elapsed,
        );
        drop(registry);
    }

    /// Complete the readiness barrier for an actor whose authoritative state
    /// has just been read from its mailbox.
    ///
    /// The detached startup task remains a fallback, but callers that promise
    /// a materialized actor must not return before the current UID owns the
    /// latest durable timeout decision.
    pub(crate) fn reconcile_ready_actor_state_timeout(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        actor_uid: uuid::Uuid,
        response: &EntityResponse,
    ) {
        self.arm_state_timeouts_on_current_actor_hydration(
            tenant,
            entity_type,
            entity_id,
            actor_uid,
            response,
            StateTimeoutHydrationTiming {
                observed_at: sim_now(),
                readiness_elapsed: Duration::ZERO,
            },
        );
    }

    fn arm_state_timeouts_on_hydration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        response: &EntityResponse,
        observed_at: DateTime<Utc>,
        readiness_elapsed: Duration,
    ) {
        let agent_ctx = crate::request_context::AgentContext::for_service(STATE_TIMEOUT_SERVICE);
        let action_params = serde_json::json!({});
        let ctx = PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action: "__hydrated",
            agent_ctx: &agent_ctx,
            dispatch_idempotency_key: None,
            action_params: &action_params,
            await_integration: false,
            actor_uid: None,
        };
        self.arm_state_timeouts(
            &ctx,
            response,
            StateTimeoutArmCause::Hydration {
                observed_at,
                readiness_elapsed,
            },
        );
    }

    /// Reconcile a durable state produced outside its actor mailbox before the
    /// successful commit returns to the caller.
    ///
    /// Atomic composite writes and initial File content writes deliberately
    /// bypass actor dispatch. Reusing hydration reconciliation establishes
    /// timeout ownership before any stale actor is drained: a committed timed
    /// state owns a live timer, while a committed untimed or deleted state
    /// fences callbacks from the pre-commit incarnation. The durable clock
    /// remains authoritative, and a concurrently replaced or removed
    /// declaration is resolved against the current table by the shared
    /// scheduler.
    pub(crate) fn reconcile_state_timeout_after_synthetic_commit(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        committed_state: &EntityState,
    ) -> Option<InactiveStateTimeoutFence> {
        let event_order = timeout_response_order(committed_state);
        if committed_state.status == "Deleted"
            || self
                .current_state_timeout_declaration(tenant, entity_type, &committed_state.status)
                .is_none()
        {
            let key = EntityKey::new(tenant, entity_type, entity_id);
            return self.state_timeout_tracker.fence_inactive_if_fresh(
                &key,
                event_order,
                committed_state.state_timeout_clock_reset_at,
                committed_state.state_timeout_clock_reset_version,
            );
        }
        let response = EntityResponse {
            success: true,
            state: committed_state.clone(),
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        };
        self.arm_state_timeouts_on_hydration(
            tenant,
            entity_type,
            entity_id,
            &response,
            sim_now(),
            Duration::ZERO,
        );
        None
    }

    /// Fence every timeout owner immediately after an out-of-band commit and
    /// before any stale actor can be awaited or evicted.
    ///
    /// Timed states are deliberately fenced inactive here. Their replacement
    /// actor (or the synthetic no-actor path) installs the active owner only
    /// after all captured stale actors have drained.
    pub(crate) fn fence_state_timeout_before_actor_eviction(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        committed_state: &EntityState,
    ) -> Option<InactiveStateTimeoutFence> {
        let key = EntityKey::new(tenant, entity_type, entity_id);
        self.state_timeout_tracker.fence_inactive_if_fresh(
            &key,
            timeout_response_order(committed_state),
            committed_state.state_timeout_clock_reset_at,
            committed_state.state_timeout_clock_reset_version,
        )
    }

    /// Fence timeout ownership after discovering a terminal durable envelope.
    pub(crate) fn fence_state_timeout_after_terminal_event(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        event_order: u64,
    ) -> Option<InactiveStateTimeoutFence> {
        let key = EntityKey::new(tenant, entity_type, entity_id);
        self.state_timeout_tracker
            .fence_inactive_if_fresh(&key, event_order, None, None)
    }

    /// Release an inactive timeout tombstone after its actor incarnation has
    /// been evicted. Eviction fences delayed hydration and post-dispatch work;
    /// the owner can therefore be removed without allowing an older response
    /// to reclaim it. Active timed owners remain intact across passivation.
    pub(crate) fn release_inactive_state_timeout_after_actor_eviction(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fence: Option<InactiveStateTimeoutFence>,
    ) {
        let Some(fence) = fence else {
            return;
        };
        let key = EntityKey::new(tenant, entity_type, entity_id);
        let _ = self
            .state_timeout_tracker
            .forget_inactive_if_current(&key, fence);
    }
}
