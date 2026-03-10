//! Post-dispatch effect handlers.
//!
//! Isolates side effects (telemetry, SSE, cache, webhooks, WASM, spawn)
//! that run after a successful entity action dispatch. Keeps the core
//! dispatch path focused on transition execution.

use std::sync::Arc;

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

impl crate::state::ServerState {
    /// Record a trajectory entry for a completed dispatch (success or guard failure).
    pub(crate) async fn record_dispatch_trajectory(
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
            from_status: response.state.events.last().map(|e| e.from_status.clone()),
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
        };
        if let Err(e) = self.persist_trajectory_entry(&entry).await {
            tracing::error!(error = %e, "failed to persist trajectory entry");
        }
    }

    /// Broadcast state change to SSE subscribers and update entity cache.
    pub(crate) fn broadcast_state_change(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let _ = self.event_tx.send(EntityStateChange {
            entity_type: ctx.entity_type.to_string(),
            entity_id: ctx.entity_id.to_string(),
            action: ctx.action.to_string(),
            status: response.state.status.clone(),
            tenant: ctx.tenant.to_string(),
            agent_id: ctx.agent_ctx.agent_id.clone(),
            session_id: ctx.agent_ctx.session_id.clone(),
        });
        let cache_key = format!("{}:{}:{}", ctx.tenant, ctx.entity_type, ctx.entity_id);
        if let Ok(mut cache) = self.entity_state_cache.write() {
            cache.insert(cache_key, (response.state.status.clone(), sim_now()));
        }
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
                from_status: response.state.events.last().map(|e| e.from_status.clone()),
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
            };
            tokio::spawn(async move {
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
            tokio::spawn(
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
                .instrument(tracing::info_span!("dispatch.scheduled_actions")),
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

        // 2. Record trajectory
        self.record_dispatch_trajectory(ctx, &response).await;

        if !response.success {
            return response;
        }

        // 3. Broadcast SSE + cache
        self.broadcast_state_change(ctx, &response);

        // 4. Fire webhooks
        self.fire_webhooks(ctx, &response);

        // 5. WASM integrations
        if !response.custom_effects.is_empty() {
            if ctx.await_integration {
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
                if let Ok(Some(final_response)) =
                    Box::pin(self.dispatch_wasm_integrations_internal(&req)).await
                {
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

        response
    }
}
