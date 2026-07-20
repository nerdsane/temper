//! Ordered post-dispatch effect orchestration.

use super::*;

impl crate::state::ServerState {
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

        // The journal response is already durable. Establish timeout
        // ownership before any fallible integration can return an error or
        // consume an unbounded portion of the remaining deadline.
        self.arm_state_timeouts_if_needed(ctx, &response);

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

                let req = super::super::WasmDispatchRequest {
                    tenant: ctx.tenant,
                    entity_type: ctx.entity_type,
                    entity_id: ctx.entity_id,
                    action: ctx.action,
                    custom_effects: &response.custom_effects,
                    entity_state: &response.state,
                    agent_ctx: ctx.agent_ctx,
                    dispatch_idempotency_key: ctx.dispatch_idempotency_key,
                    action_params: ctx.action_params,
                    mode: super::super::WasmDispatchMode::Inline,
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
                let adapter_req = super::super::WasmDispatchRequest {
                    tenant: ctx.tenant,
                    entity_type: ctx.entity_type,
                    entity_id: ctx.entity_id,
                    action: ctx.action,
                    custom_effects: &response.custom_effects,
                    entity_state: adapter_state,
                    agent_ctx: ctx.agent_ctx,
                    dispatch_idempotency_key: None,
                    action_params: ctx.action_params,
                    mode: super::super::WasmDispatchMode::Inline,
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
                self.dispatch_adapter_integrations(super::super::adapter::AdapterDispatchInput {
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

        // 8. Enqueue durable query-plane maintenance (ADR-0148). The journal
        // append is already durable; projection writes are derived rows and
        // are coalesced by entity/sequence before DB access.
        self.apply_query_projection_update(ctx, &response).await;

        response
    }
}
