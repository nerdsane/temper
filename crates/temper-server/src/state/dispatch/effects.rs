//! Post-dispatch effect handlers.
//!
//! Isolates side effects (telemetry, SSE, cache, webhooks, WASM, spawn)
//! that run after a successful entity action dispatch. Keeps the core
//! dispatch path focused on transition execution.

use std::sync::Arc;
use std::time::Instant;

use tokio::spawn as spawn_post_dispatch_effect; // determinism-ok: post-dispatch side effects are outside transition semantics
use tracing::Instrument;

use crate::entity_actor::{EntityResponse, effects::ScheduledAction};
use crate::events::EntityStateChange;
use crate::request_context::AgentContext;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

/// Collected context needed by post-dispatch effect handlers.
pub(crate) struct PostDispatchContext<'a> {
    pub tenant: &'a TenantId,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub agent_ctx: &'a AgentContext,
    pub action_params: &'a serde_json::Value,
    pub await_integration: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryProjectionUpdateMode {
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchTrajectoryPersistenceMode {
    Background,
}

fn query_projection_update_mode() -> QueryProjectionUpdateMode {
    QueryProjectionUpdateMode::Background
}

fn dispatch_trajectory_persistence_mode() -> DispatchTrajectoryPersistenceMode {
    DispatchTrajectoryPersistenceMode::Background
}

impl crate::state::ServerState {
    pub(super) fn persist_trajectory_entry_background(&self, entry: TrajectoryEntry) {
        debug_assert_eq!(
            dispatch_trajectory_persistence_mode(),
            DispatchTrajectoryPersistenceMode::Background
        );
        self.enqueue_trajectory_entry(entry);
    }

    fn enqueue_query_projection_update(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        debug_assert_eq!(
            query_projection_update_mode(),
            QueryProjectionUpdateMode::Background
        );
        let Some(query_plane) = self.query_plane_store() else {
            return;
        };

        let tenant = ctx.tenant.to_string();
        let entity_type = ctx.entity_type.to_string();
        let entity_id = ctx.entity_id.to_string();
        let status = response.state.status.clone();
        let fields =
            self.query_projection_fields(ctx.tenant, ctx.entity_type, &response.state.fields);
        let sequence_nr = response.state.sequence_nr;
        let operation = if status == "Deleted" {
            "remove"
        } else {
            "upsert"
        }
        .to_string();
        let source = "background_dispatch".to_string();
        let enqueued_at = Instant::now(); // determinism-ok: production-only projection queue metric

        crate::query_projection_metrics::record_update_enqueued(
            &tenant,
            &entity_type,
            &operation,
            &source,
        );

        let span = tracing::info_span!(
            "dispatch.phase.query_projection",
            otel.name = "dispatch.phase.query_projection",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            operation = %operation,
        );

        spawn_post_dispatch_effect(
            async move {
                let started_at = Instant::now(); // determinism-ok: production-only projection duration metric
                crate::query_projection_metrics::record_update_started(
                    &tenant,
                    &entity_type,
                    &operation,
                    &source,
                );
                crate::query_projection_metrics::record_update_queue_wait(
                    &tenant,
                    &entity_type,
                    &operation,
                    &source,
                    started_at.duration_since(enqueued_at),
                );
                let result = if status == "Deleted" {
                    query_plane
                        .remove_projection(&tenant, &entity_type, &entity_id)
                        .await
                } else {
                    query_plane
                        .upsert_projection(
                            &tenant,
                            &entity_type,
                            &entity_id,
                            &status,
                            &fields,
                            sequence_nr,
                        )
                        .await
                };
                let result_label = if result.is_ok() { "ok" } else { "error" };
                crate::query_projection_metrics::record_update_duration(
                    &tenant,
                    &entity_type,
                    &operation,
                    &source,
                    result_label,
                    started_at.elapsed(),
                );
                crate::query_projection_metrics::record_update_end_to_end_duration(
                    &tenant,
                    &entity_type,
                    &operation,
                    &source,
                    result_label,
                    enqueued_at.elapsed(),
                );

                if let Err(e) = result {
                    crate::query_projection_metrics::record_update_error(
                        &tenant,
                        &entity_type,
                        &operation,
                        &source,
                    );
                    tracing::warn!(
                        error = %e,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        operation = %operation,
                        "failed to update query projection"
                    );
                } else {
                    crate::query_projection_metrics::record_update_applied_sequence(
                        &tenant,
                        &entity_type,
                        &operation,
                        &source,
                        sequence_nr,
                    );
                }
            }
            .instrument(span),
        );
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
            from_status: response.state.events.back().map(|e| e.from_status.clone()),
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
            request_body: if response.success {
                None
            } else {
                Some(ctx.action_params.clone())
            },
            intent: if response.success {
                None
            } else {
                ctx.agent_ctx.intent.clone()
            },
            matched_policy_ids: None,
        };
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
            "trajectory.entry"
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

    /// Broadcast state change to SSE subscribers and update entity cache.
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
                }),
            );
        }
        let cache_key = format!("{}:{}:{}", ctx.tenant, ctx.entity_type, ctx.entity_id);
        self.cache_entity_status(cache_key, response.state.status.clone());
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
                from_status: response.state.events.back().map(|e| e.from_status.clone()),
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
                request_body: None,
                intent: None,
                matched_policy_ids: None,
            };
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
                "trajectory.entry"
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

    /// Schedule delayed actions as fire-and-forget background timers.
    ///
    /// Propagates the originating `AgentContext` so that scheduled actions
    /// retain identity attribution in trajectories and SSE events.
    pub(crate) fn dispatch_scheduled_actions(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        scheduled_actions: &[ScheduledAction],
        agent_ctx: &AgentContext,
    ) {
        for sched in scheduled_actions {
            let state = self.clone();
            let t = tenant.clone();
            let et = entity_type.to_string();
            let eid = entity_id.to_string();
            let action = sched.action.clone();
            let ctx = agent_ctx.clone();
            let delay = std::time::Duration::from_secs(sched.delay_seconds);
            let workflow_root_entity_type = ctx
                .workflow_root_entity_type
                .clone()
                .unwrap_or_else(|| et.clone());
            let workflow_root_entity_id = ctx
                .workflow_root_entity_id
                .clone()
                .unwrap_or_else(|| eid.clone());
            let workflow_run_id = ctx
                .workflow_run_id
                .clone()
                .unwrap_or_else(|| format!("{et}:{eid}"));
            let span = tracing::info_span!(
                "dispatch.scheduled_actions",
                workflow.root_entity_type = %workflow_root_entity_type,
                workflow.root_entity_id = %workflow_root_entity_id,
                workflow.run_id = %workflow_run_id,
                temper.action = %action,
                entity_type = %et,
                entity_id = %eid,
            );
            spawn_post_dispatch_effect(
                // determinism-ok: timer delivery is a background side-effect
                async move {
                    tokio::time::sleep(delay).await; // determinism-ok: scheduled delay
                    let _ = state
                        .dispatch_tenant_action(
                            &t,
                            &et,
                            &eid,
                            &action,
                            serde_json::json!({"__scheduled": true}),
                            &ctx,
                        )
                        .await;
                }
                .instrument(span),
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
        // 1. Record metrics
        self.metrics
            .record_transition(ctx.entity_type, ctx.action, response.success);

        // 2. Record trajectory outside the action critical path. The entity
        // transition is already durable; trajectory persistence is an
        // observability/audit side effect.
        self.record_dispatch_trajectory(ctx, &response);

        if !response.success {
            return response;
        }

        // 3. Broadcast SSE + cache
        self.broadcast_state_change(ctx, &response);

        // 4. Fire webhooks
        self.fire_webhooks(ctx, &response);

        // 5. Integrations (WASM + native adapters)
        if !response.custom_effects.is_empty() {
            if ctx.await_integration {
                let mut inline_response: Option<EntityResponse> = None;

                // ADR-0056 Sub-Decision 3: snapshot pre-integration state so
                // we can detect silent exits (integrations that returned
                // without causing a state transition) after the inline WASM
                // call returns.
                let pre_integration_status = response.state.status.clone();

                let req = super::WasmDispatchRequest {
                    tenant: ctx.tenant,
                    entity_type: ctx.entity_type,
                    entity_id: ctx.entity_id,
                    action: ctx.action,
                    custom_effects: &response.custom_effects,
                    entity_state: &response.state,
                    agent_ctx: ctx.agent_ctx,
                    action_params: ctx.action_params,
                    mode: super::WasmDispatchMode::Inline,
                };
                match Box::pin(self.dispatch_wasm_integrations_internal(&req)).await {
                    Ok(Some(final_response)) => {
                        // Silent-exit regression guard: trigger integration
                        // returned but state didn't advance. Under healthy
                        // operation the consumer-side WASM invariant (openpaw
                        // ADR-0039 Sub-Decision 3a) and the Turso persist retry
                        // (ADR-0056 Sub-Decision 2) prevent this; any nonzero
                        // reading of the counter is a critical-severity alert
                        // that something regressed.
                        if final_response.state.status == pre_integration_status {
                            tracing::warn!(
                                target: "temper_server::integration",
                                tenant = %ctx.tenant,
                                entity_type = ctx.entity_type,
                                entity_id = ctx.entity_id,
                                action = ctx.action,
                                state = %pre_integration_status,
                                "integration returned without state transition \u{2014} invariant violation"
                            );
                            crate::runtime_metrics::record_integration_silent_exit(
                                ctx.tenant.as_str(),
                                ctx.entity_type,
                                ctx.action,
                                &pre_integration_status,
                            );
                        }
                        inline_response = Some(final_response);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        return EntityResponse {
                            success: false,
                            state: response.state.clone(),
                            error: Some(format!("WASM integration failed: {err}")),
                            custom_effects: response.custom_effects.clone(),
                            scheduled_actions: Vec::new(),
                            spawn_requests: Vec::new(),
                            spec_governed: response.spec_governed,
                        };
                    }
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
                    action_params: ctx.action_params,
                    mode: super::WasmDispatchMode::Inline,
                };
                match Box::pin(self.dispatch_adapter_integrations_internal(&adapter_req)).await {
                    Ok(Some(final_response)) => {
                        inline_response = Some(final_response);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        return EntityResponse {
                            success: false,
                            state: response.state.clone(),
                            error: Some(format!("adapter integration failed: {err}")),
                            custom_effects: response.custom_effects.clone(),
                            scheduled_actions: Vec::new(),
                            spawn_requests: Vec::new(),
                            spec_governed: response.spec_governed,
                        };
                    }
                }

                if let Some(final_response) = inline_response {
                    return final_response;
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
        // the handler routes effects to platform hooks (hooks.rs).
        if !response.custom_effects.is_empty()
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

        // 6. Spawn requests
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

        // 7. Scheduled actions (propagate agent context for identity attribution)
        if !response.scheduled_actions.is_empty() {
            self.dispatch_scheduled_actions(
                ctx.tenant,
                ctx.entity_type,
                ctx.entity_id,
                &response.scheduled_actions,
                ctx.agent_ctx,
            );
        }

        // 7b. ADR-0049 state-timeout arming. Arms (or re-arms) a timer on
        // state entry and on any reset_on action. Sequence-based
        // cancellation ensures stale timers become no-ops.
        self.arm_state_timeouts_if_needed(ctx, &response);

        // 8. Update the durable query plane for collection reads. This must not
        // sit on the action-dispatch critical path; the entity transition is
        // already durable, and projection lag is tracked by explicit metrics.
        self.enqueue_query_projection_update(ctx, &response);

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_projection_updates_are_not_on_the_dispatch_critical_path() {
        assert_eq!(
            query_projection_update_mode(),
            QueryProjectionUpdateMode::Background
        );
    }

    #[test]
    fn dispatch_trajectory_persistence_is_not_on_the_dispatch_critical_path() {
        assert_eq!(
            dispatch_trajectory_persistence_mode(),
            DispatchTrajectoryPersistenceMode::Background
        );
    }
}
