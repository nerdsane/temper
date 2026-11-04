//! Post-dispatch effect handlers.
//!
//! Isolates side effects (telemetry, SSE, cache, webhooks, WASM, spawn)
//! that run after a successful entity action dispatch. Keeps the core
//! dispatch path focused on transition execution.

use std::sync::Arc;

use tokio::spawn as spawn_post_dispatch_effect; // determinism-ok: post-dispatch side effects are outside transition semantics

use crate::entity_actor::EntityResponse;
use crate::events::EntityStateChange;
use crate::request_context::AgentContext;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

/// The only tenant whose entities reach the platform custom effect handler:
/// the system tenant, where the platform installs its own specs.
const PLATFORM_HOOK_TENANT: &str = "temper-system";

/// Collected context needed by post-dispatch effect handlers.
pub(crate) struct PostDispatchContext<'a> {
    pub tenant: &'a TenantId,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub agent_ctx: &'a AgentContext,
    pub dispatch_idempotency_key: Option<&'a str>,
    pub action_params: &'a serde_json::Value,
    pub await_integration: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchTrajectoryPersistenceMode {
    Background,
}

fn dispatch_trajectory_persistence_mode() -> DispatchTrajectoryPersistenceMode {
    DispatchTrajectoryPersistenceMode::Background
}

/// The state the action was attempted from.
///
/// A successful dispatch appends an event whose `from_status` is exactly that
/// state, so the newest event answers the question; a successful move that
/// wrote none says it itself (`EntityResponse::move_start`). A rejected one appends no
/// event (`entity_actor::effects::process_action` returns `event: None` on a
/// guard or state mismatch), so the newest event belongs to the *previous*
/// successful transition and names a state the entity has already left.
///
/// Reading it anyway hides the fault the row exists to record: a second
/// `SubmitOrder` refused in `Submitted` would be stored as attempted from
/// `Draft`, where it is legal, and every downstream reader — the conformance
/// checker first among them — would see a legal source state for an illegal
/// retry. A rejected action changes nothing, so the entity's current status is
/// where the attempt was made.
fn source_status(response: &EntityResponse) -> Option<String> {
    if response.success {
        response.move_start().map(|(from, _)| from.to_string())
    } else {
        Some(response.state.status.clone())
    }
}

impl crate::state::ServerState {
    pub(super) fn persist_trajectory_entry_background(&self, entry: TrajectoryEntry) {
        debug_assert_eq!(
            dispatch_trajectory_persistence_mode(),
            DispatchTrajectoryPersistenceMode::Background
        );
        self.enqueue_trajectory_entry(entry);
    }

    /// Record a trajectory entry for a completed dispatch (success or guard failure).
    pub(crate) fn record_dispatch_trajectory(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: ctx.tenant.to_string(),
            entity_type: ctx.entity_type.to_string(),
            entity_id: ctx.entity_id.to_string(),
            action: ctx.action.to_string(),
            success: response.success,
            from_status: source_status(response),
            to_status: Some(response.state.status.clone()),
            error: if response.success {
                None
            } else {
                Some(
                    response
                        .error
                        .clone()
                        .unwrap_or_else(|| "guard not met".to_string()),
                )
            },
            agent_id: ctx.agent_ctx.agent_id.clone(),
            session_id: ctx.agent_ctx.session_id.clone(),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some(TrajectorySource::Entity),
            spec_governed: None,
            agent_type: ctx.agent_ctx.agent_type.clone(),
            // Captured on success as well as failure. A trajectory that
            // records only what failed cannot be replayed or learned from:
            // the successful action and the arguments that produced it are
            // exactly the signal RL and replay consumers need. Size is bounded
            // when the entry is enqueued (see `enqueue_trajectory_entry`).
            request_body: Some(ctx.action_params.clone()),
            intent: ctx.agent_ctx.intent.clone(),
            matched_policy_ids: None,
            capture_seq: None,
        };
        let from_status = entry.from_status.as_deref().unwrap_or("unknown");
        let to_status = entry.to_status.as_deref().unwrap_or("unknown");
        let outcome = if entry.success { "succeeded" } else { "failed" };
        let observation_metadata = ctx
            .agent_ctx
            .observation_metadata_json()
            .unwrap_or_default();
        tracing::info!(
            tenant = %entry.tenant,
            entity_type = %entry.entity_type,
            entity_id = %entry.entity_id,
            action = %entry.action,
            success = entry.success,
            from_status = ?entry.from_status,
            to_status = ?entry.to_status,
            error = ?entry.error,
            source = ?entry.source,
            authz_denied = ?entry.authz_denied,
            agent_id = entry.agent_id.as_deref().unwrap_or(""),
            session_id = entry.session_id.as_deref().unwrap_or(""),
            agent_type = entry.agent_type.as_deref().unwrap_or(""),
            intent = entry.intent.as_deref().unwrap_or(""),
            observation_metadata = %observation_metadata,
            "app usage: {}.{} {} -> {} on {} {}",
            entry.entity_type,
            entry.action,
            from_status,
            to_status,
            entry.entity_id,
            outcome
        );
        if !entry.success {
            tracing::warn!(
                tenant = %entry.tenant,
                entity_type = %entry.entity_type,
                entity_id = %entry.entity_id,
                action = %entry.action,
                error = ?entry.error,
                authz_denied = ?entry.authz_denied,
                source = ?entry.source,
                "unmet_intent"
            );
        }
        self.persist_trajectory_entry_background(entry);
    }

    /// Broadcast state change to SSE subscribers.
    pub(crate) fn broadcast_state_change(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let seq =
            self.next_entity_event_sequence(ctx.tenant.as_str(), ctx.entity_type, ctx.entity_id);
        let change = EntityStateChange {
            seq,
            entity_type: ctx.entity_type.to_string(),
            entity_id: ctx.entity_id.to_string(),
            action: ctx.action.to_string(),
            status: response.state.status.clone(),
            tenant: ctx.tenant.to_string(),
            agent_id: ctx.agent_ctx.agent_id.clone(),
            session_id: ctx.agent_ctx.session_id.clone(),
            intent: ctx.agent_ctx.intent.clone(),
            observation_metadata: (!ctx.agent_ctx.observation_metadata.is_empty())
                .then(|| ctx.agent_ctx.observation_metadata.clone()),
        };
        self.record_entity_observe_event_with_seq(
            ctx.tenant.as_str(),
            ctx.entity_type,
            ctx.entity_id,
            seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let _ = self.event_tx.send(change);
        if matches!(
            response.state.status.as_str(),
            "Completed" | "Failed" | "Cancelled"
        ) {
            let terminal_seq = self.next_entity_event_sequence(
                ctx.tenant.as_str(),
                ctx.entity_type,
                ctx.entity_id,
            );
            let result = response
                .state
                .fields
                .get("result")
                .or_else(|| response.state.fields.get("Result"))
                .and_then(serde_json::Value::as_str);
            let error_message = response
                .state
                .fields
                .get("error_message")
                .or_else(|| response.state.fields.get("ErrorMessage"))
                .and_then(serde_json::Value::as_str)
                .or(response.error.as_deref());
            self.record_entity_observe_event_with_seq(
                ctx.tenant.as_str(),
                ctx.entity_type,
                ctx.entity_id,
                terminal_seq,
                "agent_complete",
                serde_json::json!({
                    "seq": terminal_seq,
                    "status": response.state.status,
                    "action": ctx.action,
                    "result": result,
                    "error_message": error_message,
                    "agent_id": ctx.agent_ctx.agent_id,
                    "session_id": ctx.agent_ctx.session_id,
                    "observation_metadata": ctx.agent_ctx.observation_metadata.clone(),
                }),
            );
        }
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Entities);
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Trajectories);
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Agents);
    }

    /// Fire webhooks for the trajectory entry (non-blocking).
    pub(crate) fn fire_webhooks(&self, ctx: &PostDispatchContext<'_>, response: &EntityResponse) {
        if let Some(ref dispatcher) = self.webhook_dispatcher {
            let dispatcher = Arc::clone(dispatcher);
            let entry = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: ctx.tenant.to_string(),
                entity_type: ctx.entity_type.to_string(),
                entity_id: ctx.entity_id.to_string(),
                action: ctx.action.to_string(),
                success: response.success,
                from_status: source_status(response),
                to_status: Some(response.state.status.clone()),
                error: response.error.clone(),
                agent_id: ctx.agent_ctx.agent_id.clone(),
                session_id: ctx.agent_ctx.session_id.clone(),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Entity),
                spec_governed: None,
                agent_type: ctx.agent_ctx.agent_type.clone(),
                // Deliberately absent, unlike the persisted trajectory entry:
                // webhook payloads leave the deployment, so action arguments
                // are not egressed to third-party endpoints.
                request_body: None,
                intent: ctx.agent_ctx.intent.clone(),
                matched_policy_ids: None,
                capture_seq: None,
            };
            let from_status = entry.from_status.as_deref().unwrap_or("unknown");
            let to_status = entry.to_status.as_deref().unwrap_or("unknown");
            let outcome = if entry.success { "succeeded" } else { "failed" };
            let observation_metadata = ctx
                .agent_ctx
                .observation_metadata_json()
                .unwrap_or_default();
            tracing::info!(
                tenant = %entry.tenant,
                entity_type = %entry.entity_type,
                entity_id = %entry.entity_id,
                action = %entry.action,
                success = entry.success,
                from_status = ?entry.from_status,
                to_status = ?entry.to_status,
                error = ?entry.error,
                source = ?entry.source,
                authz_denied = ?entry.authz_denied,
                agent_id = entry.agent_id.as_deref().unwrap_or(""),
                session_id = entry.session_id.as_deref().unwrap_or(""),
                agent_type = entry.agent_type.as_deref().unwrap_or(""),
                intent = entry.intent.as_deref().unwrap_or(""),
                observation_metadata = %observation_metadata,
                "app usage: {}.{} {} -> {} on {} {}",
                entry.entity_type,
                entry.action,
                from_status,
                to_status,
                entry.entity_id,
                outcome
            );
            if !entry.success {
                tracing::warn!(
                    tenant = %entry.tenant,
                    entity_type = %entry.entity_type,
                    entity_id = %entry.entity_id,
                    action = %entry.action,
                    error = ?entry.error,
                    authz_denied = ?entry.authz_denied,
                    source = ?entry.source,
                    "unmet_intent"
                );
            }
            spawn_post_dispatch_effect(async move {
                // determinism-ok: external side-effect, no simulation-visible state
                dispatcher.dispatch(&entry);
            });
        }
    }

    /// Dispatch a transition's deferred child effects: its spawn requests,
    /// and arm its scheduled actions (their rows were written with its event).
    ///
    /// These belong to the ORIGINAL transition response and must run on every
    /// successful path — including when an inline integration produced a
    /// callback response. They are fire-and-forget (background tasks), so the
    /// caller is free to return the (possibly integration-updated) response
    /// afterwards.
    pub(super) fn dispatch_transition_deferred_effects(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        self.arm_scheduled_actions(ctx, response);
        // Spawn requests
        if !response.spawn_requests.is_empty() {
            self.dispatch_spawn_requests(
                ctx.tenant,
                ctx.entity_type,
                ctx.entity_id,
                &response.spawn_requests,
                ctx.action_params,
                ctx.agent_ctx,
            );
        }
    }

    /// Run all post-dispatch effects for a successful action.
    ///
    /// This is the single orchestration point for side effects after a
    /// transition executes. Returns a potentially updated response (e.g.
    /// if blocking WASM integration produced a new state).
    pub(crate) async fn run_post_dispatch_effects(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: EntityResponse,
    ) -> EntityResponse {
        // A write refused because its record changed was not decided: its
        // route decides it again, and that decision is the one recorded.
        if response.is_record_changed() {
            return response;
        }
        // A scheduled action that came due where its action has no
        // transition did not fire, and nothing is recorded for it.
        if super::scheduled_actions::scheduled_fire_lapsed(
            ctx.agent_ctx.fires_scheduled_action.as_ref(),
            &response,
        ) {
            return response;
        }

        // 1. Record metrics
        self.metrics
            .record_transition(ctx.entity_type, ctx.action, response.success);

        // 2. Record trajectory outside the action critical path. The entity
        // transition is already durable; trajectory persistence is an
        // observability/audit side effect. A move that wrote nothing (it
        // left its record as it was) has no history to add.
        let recorded = response.unrecorded_at.is_none();
        if recorded {
            self.record_dispatch_trajectory(ctx, &response);
        }

        if !response.success {
            return response;
        }

        // Cancellation is bookkeeping for the committed transition, not a
        // dependent launch. An old reset timer must not fire during the wait.
        self.invalidate_state_timeouts_if_needed(ctx, &response);

        // The status is committed, so guards decide on it from here on, even
        // while the projection below is still being written. Simulation runs
        // other requests between the commit and this write, as a slow writer
        // would.
        temper_runtime::buggify::buggify_yield(
            crate::state::status_cache::STATUS_WRITE_YIELD_BUDGET,
        )
        .await;
        self.cache_entity_status(ctx.tenant, &response.state);

        // The state timer the transition entered or reset is bookkeeping for
        // the commit, like the cancellation above: it fires through the
        // record's actor and reads nothing from the query plane, so it starts
        // now rather than after the projection. A projection that lags or
        // fails must not leave a committed record without its timer. Boxed:
        // the timer row's write inline overflowed a test thread's stack once
        // dispatch carried request traces.
        Box::pin(self.arm_state_timeouts_if_needed(ctx, &response)).await;

        // The transition's query-plane row committed with its event, so the
        // caller, a reaction or an integration that reads it at once finds it.

        // 3. Broadcast SSE
        self.broadcast_state_change(ctx, &response);

        // 4. Fire webhooks: each sends the trajectory entry of a recorded move.
        if recorded {
            self.fire_webhooks(ctx, &response);
        }

        // 5. Integrations (WASM + native adapters)
        if !response.custom_effects.is_empty() {
            if ctx.await_integration {
                let mut inline_response: Option<EntityResponse> = None;

                let req = super::WasmDispatchRequest {
                    tenant: ctx.tenant,
                    entity_type: ctx.entity_type,
                    entity_id: ctx.entity_id,
                    action: ctx.action,
                    custom_effects: &response.custom_effects,
                    entity_state: &response.state,
                    agent_ctx: ctx.agent_ctx,
                    dispatch_idempotency_key: ctx.dispatch_idempotency_key,
                    action_params: ctx.action_params,
                    mode: super::WasmDispatchMode::Inline,
                };
                // Each trigger runs whether or not one before it failed, so a
                // failure is kept and the triggers after it still run.
                let mut failures: Vec<String> = Vec::new();
                match super::wasm::dispatch_wasm_integrations_boxed(self, &req).await {
                    Ok(Some(final_response)) => {
                        inline_response = Some(final_response);
                    }
                    Ok(None) => {}
                    Err(err) => failures.push(format!("WASM integration failed: {err}")),
                }

                let adapter_state = inline_response
                    .as_ref()
                    .map(|r| &r.state)
                    .unwrap_or(&response.state);
                let adapter_req = super::WasmDispatchRequest {
                    tenant: ctx.tenant,
                    entity_type: ctx.entity_type,
                    entity_id: ctx.entity_id,
                    action: ctx.action,
                    custom_effects: &response.custom_effects,
                    entity_state: adapter_state,
                    agent_ctx: ctx.agent_ctx,
                    dispatch_idempotency_key: None,
                    action_params: ctx.action_params,
                    mode: super::WasmDispatchMode::Inline,
                };
                match Box::pin(self.dispatch_adapter_integrations_internal(&adapter_req)).await {
                    Ok(Some(final_response)) => {
                        inline_response = Some(final_response);
                    }
                    Ok(None) => {}
                    Err(err) => failures.push(format!("adapter integration failed: {err}")),
                }

                if !failures.is_empty()
                    || inline_response
                        .as_ref()
                        .is_some_and(|answer| !answer.success)
                {
                    return self.answer_failed_triggers(ctx, response, inline_response, failures);
                }
                if let Some(final_response) = inline_response {
                    // ARN-188: the inline integration produced a callback
                    // response (a separate action), but the ORIGINAL
                    // transition's spawn requests and scheduled actions still
                    // need to run — the previous early return dropped them
                    // silently. Dispatch them before returning the
                    // integration-updated response.
                    //
                    // Platform hooks (5b) stay skipped on this path: the
                    // original custom effects already went to the inline
                    // integration, and routing them to the hooks too would
                    // handle them twice. The projection and the state-timer
                    // arm ran above, before the integration.
                    self.dispatch_transition_deferred_effects(ctx, &response);
                    // The request's own action committed. If the store did not
                    // take the answer, saying so (503) would ask the caller to
                    // send the action again, and it would apply twice.
                    return EntityResponse {
                        store_unavailable: false,
                        ..final_response
                    };
                }
            } else {
                self.dispatch_wasm_integrations(
                    ctx.tenant,
                    ctx.entity_type,
                    ctx.entity_id,
                    ctx.action,
                    &response.custom_effects,
                    &response.state,
                    ctx.agent_ctx,
                    ctx.action_params,
                );
                self.dispatch_adapter_integrations(super::adapter::AdapterDispatchInput {
                    tenant: ctx.tenant,
                    entity_type: ctx.entity_type,
                    entity_id: ctx.entity_id,
                    action: ctx.action,
                    custom_effects: &response.custom_effects,
                    entity_state: &response.state,
                    agent_ctx: ctx.agent_ctx,
                    action_params: ctx.action_params,
                });
            }
        }

        // 5b. Platform custom effect hooks
        //
        // For system-tenant entities whose specs declare custom effects
        // but have no WASM/adapter integrations (e.g. GovernanceDecision),
        // the handler routes effects to platform hooks (hooks.rs). The
        // platform installs those specs only in the system tenant, and the
        // hooks act on whatever tenant and entity the record names, so only
        // system-tenant entities reach them. A hook name in another tenant's
        // spec is an ordinary custom effect for that tenant's integrations.
        if !response.custom_effects.is_empty()
            && ctx.tenant.as_str() == PLATFORM_HOOK_TENANT
            && let Some(handler) = &self.custom_effect_handler
        {
            for effect_name in &response.custom_effects {
                if let Err(e) = handler.handle(
                    effect_name,
                    ctx.entity_type,
                    ctx.entity_id,
                    &response.state.fields,
                    self,
                ) {
                    tracing::error!(
                        effect = %effect_name,
                        entity_type = ctx.entity_type,
                        entity_id = ctx.entity_id,
                        error = %e,
                        "custom effect handler failed"
                    );
                }
            }
        }

        // 6 & 7. Spawn requests + scheduled actions (the transition's deferred
        // child effects). Shared with the inline-integration early-return path
        // so those effects are never dropped (ARN-188).
        self.dispatch_transition_deferred_effects(ctx, &response);

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_trajectory_persistence_is_not_on_the_dispatch_critical_path() {
        assert_eq!(
            dispatch_trajectory_persistence_mode(),
            DispatchTrajectoryPersistenceMode::Background
        );
    }
}

#[cfg(test)]
mod strict_timer_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn strict_parameterless_timer_reaches_the_native_actor() {
        let state = super::super::strict_test_support::state();
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("test-timer");
        let scheduled = state
            .dispatch_tenant_action(
                &tenant,
                "StrictJob",
                "job",
                "Schedule",
                serde_json::json!({}),
                &agent,
            )
            .await
            .unwrap();
        assert!(scheduled.success);
        let mut changes = state.event_tx.subscribe();
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        let changed = tokio::time::timeout(std::time::Duration::from_secs(1), changes.recv()).await;
        assert!(
            changed.is_ok(),
            "scheduled action did not reach the strict actor"
        );
        assert_eq!(
            super::super::strict_test_support::read(&state).await.status,
            "Done"
        );
        let rejected = state
            .dispatch_tenant_action(
                &tenant,
                "StrictJob",
                "job",
                "Poll",
                serde_json::json!({"__scheduled":true}),
                &agent,
            )
            .await
            .unwrap();
        assert!(!rejected.success);
    }
}
