use std::sync::Arc;

use crate::dispatch::AgentContext;
use crate::entity_actor::{EntityMsg, EntityResponse, EntityState};
use crate::events::EntityStateChange;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::DispatchExtOptions;

impl crate::state::ServerState {
    /// Dispatch an action to an entity actor (legacy single-tenant).
    pub async fn dispatch_action(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        self.dispatch_tenant_action(
            &TenantId::default(),
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
    }

    /// Dispatch an action to an entity actor for a specific tenant.
    ///
    /// After a successful action, also triggers any matching reaction rules
    /// for cross-entity coordination.
    pub async fn dispatch_tenant_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
    ) -> Result<EntityResponse, String> {
        self.dispatch_tenant_action_ext(
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            DispatchExtOptions {
                agent_ctx,
                await_integration: false,
            },
        )
        .await
    }

    /// Dispatch with optional blocking integration await.
    pub async fn dispatch_tenant_action_ext(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        options: DispatchExtOptions<'_>,
    ) -> Result<EntityResponse, String> {
        let agent_ctx = options.agent_ctx;
        let await_integration = options.await_integration;
        let response = self
            .dispatch_tenant_action_core(
                tenant,
                entity_type,
                entity_id,
                action,
                params,
                agent_ctx,
                await_integration,
            )
            .await?;

        // Dispatch cross-entity reactions (fire-and-forget, depth 0 = top-level)
        if response.success {
            let dispatcher = self
                .reaction_dispatcher
                .read()
                .ok()
                .and_then(|slot| slot.clone());
            if let Some(dispatcher) = dispatcher {
                let fields = serde_json::to_value(&response.state.fields).unwrap_or_default();
                dispatcher
                    .dispatch_reactions(
                        self,
                        tenant,
                        entity_type,
                        entity_id,
                        action,
                        &response.state.status,
                        &fields,
                        0,
                    )
                    .await;
            }
        }

        // Schedule delayed actions (fire-and-forget background timers).
        // Uses a sync helper (like dispatch_wasm_integrations) so
        // tokio::spawn doesn't affect the async future's Send analysis.
        if response.success && !response.scheduled_actions.is_empty() {
            self.dispatch_scheduled_actions(
                tenant,
                entity_type,
                entity_id,
                &response.scheduled_actions,
            );
        }

        Ok(response)
    }

    /// Schedule delayed actions as fire-and-forget background timers.
    ///
    /// This is a **sync** method (like `dispatch_wasm_integrations`) so that
    /// `tokio::spawn` inside it does not affect the Send analysis of the
    /// calling async function's future.
    fn dispatch_scheduled_actions(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        scheduled_actions: &[crate::entity_actor::effects::ScheduledAction],
    ) {
        for sched in scheduled_actions {
            let state = self.clone();
            let t = tenant.clone();
            let et = entity_type.to_string();
            let eid = entity_id.to_string();
            let action = sched.action.clone();
            let delay = std::time::Duration::from_secs(sched.delay_seconds);
            tokio::spawn(async move {
                // determinism-ok: timer delivery is a background side-effect
                tokio::time::sleep(delay).await;
                let _ = state
                    .dispatch_tenant_action(
                        &t,
                        &et,
                        &eid,
                        &action,
                        serde_json::json!({"__scheduled": true}),
                        &AgentContext::default(),
                    )
                    .await;
            });
        }
    }

    /// Core dispatch without reaction cascade (used by ReactionDispatcher to
    /// avoid infinite async recursion).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn dispatch_tenant_action_core(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
        await_integration: bool,
    ) -> Result<EntityResponse, String> {
        let Some(actor_ref) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) else {
            // Spec-free dispatch: no transition table, but Cedar allowed the action.
            let entry = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                action: action.to_string(),
                success: true,
                from_status: None,
                to_status: None,
                error: None,
                agent_id: agent_ctx.agent_id.clone(),
                session_id: agent_ctx.session_id.clone(),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Entity),
                spec_governed: Some(false),
            };
            if let Err(e) = self.persist_trajectory_entry(&entry).await {
                tracing::error!(error = %e, "failed to persist trajectory entry");
            }
            return Ok(EntityResponse {
                success: true,
                state: EntityState {
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    status: String::new(),
                    item_count: 0,
                    counters: std::collections::BTreeMap::new(),
                    booleans: std::collections::BTreeMap::new(),
                    lists: std::collections::BTreeMap::new(),
                    fields: serde_json::json!({}),
                    events: vec![],
                    sequence_nr: 0,
                },
                error: None,
                custom_effects: vec![],
                scheduled_actions: vec![],
                spawn_requests: vec![],
                spec_governed: false,
            });
        };

        // Pre-resolve cross-entity state gates (Gap 1: Agent OS).
        // Walk rules for this action, collect CrossEntityStateIn guards,
        // resolve target entity status, produce boolean map.
        let cross_entity_booleans = self
            .resolve_cross_entity_guards(tenant, entity_type, entity_id, action)
            .await;

        let action_params = params.clone();
        let response = match actor_ref
            .ask::<EntityResponse>(
                EntityMsg::Action {
                    name: action.to_string(),
                    params,
                    cross_entity_booleans,
                },
                self.action_dispatch_timeout,
            )
            .await
        {
            Ok(response) => response,
            Err(e) => {
                // Record a trajectory entry for actor dispatch failures.
                let entry = TrajectoryEntry {
                    timestamp: sim_now().to_rfc3339(),
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    action: action.to_string(),
                    success: false,
                    from_status: None,
                    to_status: None,
                    error: Some(format!("Actor dispatch failed: {e}")),
                    agent_id: agent_ctx.agent_id.clone(),
                    session_id: agent_ctx.session_id.clone(),
                    authz_denied: None,
                    denied_resource: None,
                    denied_module: None,
                    source: Some(TrajectorySource::Entity),
                    spec_governed: None,
                };
                if let Err(persist_err) = self.persist_trajectory_entry(&entry).await {
                    tracing::error!(error = %persist_err, "failed to persist trajectory entry");
                }
                return Err(format!("Actor dispatch failed: {e}"));
            }
        };

        // Record metrics for the /observe endpoints.
        self.metrics
            .record_transition(entity_type, action, response.success);

        // Record trajectory entry for every completed action (success or failure).
        let trajectory_entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: action.to_string(),
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
            agent_id: agent_ctx.agent_id.clone(),
            session_id: agent_ctx.session_id.clone(),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some(TrajectorySource::Entity),
            spec_governed: None,
        };
        // Best-effort persistence to event store.
        if let Err(e) = self.persist_trajectory_entry(&trajectory_entry).await {
            tracing::error!(error = %e, "failed to persist trajectory entry");
        }


        // Broadcast state change for SSE subscribers (best-effort, ignore send errors)
        if response.success {
            let _ = self.event_tx.send(EntityStateChange {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                action: action.to_string(),
                status: response.state.status.clone(),
                tenant: tenant.to_string(),
                agent_id: agent_ctx.agent_id.clone(),
                session_id: agent_ctx.session_id.clone(),
            });
            // Update entity state cache for /observe/entities
            let cache_key = format!("{tenant}:{entity_type}:{entity_id}");
            if let Ok(mut cache) = self.entity_state_cache.write() {
                cache.insert(cache_key, (response.state.status.clone(), sim_now()));
            }
        }

        // Fire webhooks (non-blocking — fire-and-forget, failure never affects response)
        if let Some(ref dispatcher) = self.webhook_dispatcher {
            let dispatcher = Arc::clone(dispatcher);
            let entry = trajectory_entry;
            tokio::spawn(async move {
                // determinism-ok: external side-effect, no simulation-visible state
                dispatcher.dispatch(&entry);
            });
        }

        // Dispatch WASM integrations for custom effects (async post-transition).
        // Each integration callback is spawned as a background task internally.
        // This matches the IOA model: output action → environment → input action.
        if response.success && !response.custom_effects.is_empty() {
            if await_integration {
                if let Ok(Some(final_response)) = self
                    .dispatch_wasm_integrations_blocking(
                        crate::state::dispatch_blocking::BlockingWasmDispatch {
                            tenant,
                            entity_type,
                            entity_id,
                            action,
                            custom_effects: &response.custom_effects,
                            entity_state: &response.state,
                            agent_ctx,
                            action_params: &action_params,
                        },
                    )
                    .await
                {
                    return Ok(final_response);
                }
            } else {
                self.dispatch_wasm_integrations(
                    tenant,
                    entity_type,
                    entity_id,
                    action,
                    &response.custom_effects,
                    &response.state,
                    agent_ctx,
                    &action_params,
                );
            }
        }

        // Dispatch entity spawn requests (Gap 2: Agent OS).
        // Same pattern as scheduled actions: fire-and-forget background tasks.
        if response.success && !response.spawn_requests.is_empty() {
            self.dispatch_spawn_requests(
                tenant,
                entity_type,
                entity_id,
                &response.spawn_requests,
                agent_ctx,
            );
        }

        Ok(response)
    }
}
