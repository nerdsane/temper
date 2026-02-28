//! Action dispatch and WASM integration methods for ServerState.

use std::sync::Arc;

use super::ServerState;
use super::pending_decisions::PendingDecision;
use super::trajectory::TrajectoryEntry;
use super::wasm_invocation_log::WasmInvocationEntry;
use crate::dispatch::AgentContext;
use crate::entity_actor::{EntityMsg, EntityResponse, EntityState};
use crate::events::EntityStateChange;
use crate::secret_template::resolve_secret_templates;
use crate::wasm_authz_gate::PermissiveWasmAuthzGate;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{
    AuthorizedWasmHost, ProductionWasmHost, WasmAuthzContext, WasmAuthzGate, WasmHost,
    WasmInvocationContext, WasmResourceLimits,
};

impl ServerState {
    /// Build a `WasmAuthzGate` for the current configuration.
    ///
    /// Returns `CedarWasmAuthzGate` if Cedar WASM gating is configured,
    /// otherwise returns `PermissiveWasmAuthzGate` for backward compatibility.
    pub(crate) fn wasm_authz_gate(&self) -> Arc<dyn WasmAuthzGate> {
        // If the authz engine has policies loaded, use Cedar gate.
        if self.authz.policy_count() > 0 {
            Arc::new(crate::wasm_authz_gate::CedarWasmAuthzGate::new(
                self.authz.clone(),
            ))
        } else {
            Arc::new(PermissiveWasmAuthzGate)
        }
    }

    /// Dispatch WASM integrations for custom effects produced by a transition.
    ///
    /// For each custom effect matching a WASM integration, this method:
    /// 1. Looks up the integration config from the spec
    /// 2. Looks up the module hash from the WASM registry
    /// 3. Invokes the WASM module via `WasmEngine`
    /// 4. Dispatches the callback action (on_success or on_failure) based on the result
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_wasm_integrations(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        _action: &str,
        custom_effects: &[String],
        _entity_state: &EntityState,
        agent_ctx: &AgentContext,
        action_params: &serde_json::Value,
    ) {
        // Look up integrations for this entity type
        let integrations = {
            let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
            registry
                .get_spec(tenant, entity_type)
                .map(|spec| spec.integrations.clone())
                .unwrap_or_default()
        };

        let gate = self.wasm_authz_gate();

        for effect_name in custom_effects {
            // Find matching WASM integration
            let matching = integrations
                .iter()
                .find(|ig| ig.integration_type == "wasm" && ig.trigger == *effect_name);

            let Some(integration) = matching else {
                continue;
            };

            let Some(ref module_name) = integration.module else {
                tracing::warn!(
                    tenant = %tenant,
                    entity_type,
                    integration = %integration.name,
                    "WASM integration missing module name"
                );
                continue;
            };

            // Check if module is registered
            let has_module = {
                let wasm_reg = self.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
                wasm_reg.get_hash(tenant, module_name).is_some()
            };

            if !has_module {
                tracing::warn!(
                    tenant = %tenant,
                    entity_type,
                    module = %module_name,
                    "WASM module not found in registry"
                );

                // Record invocation log entry for module-not-found
                let log_entry = WasmInvocationEntry {
                    timestamp: sim_now().to_rfc3339(),
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    module_name: module_name.clone(),
                    trigger_action: _action.to_string(),
                    callback_action: integration.on_failure.clone(),
                    success: false,
                    error: Some(format!("WASM module '{}' not found", module_name)),
                    duration_ms: 0,
                    authz_denied: None,
                };
                if let Ok(mut log) = self.wasm_invocation_log.write() {
                    log.push(log_entry.clone());
                }
                // Persist to DB (fire-and-forget)
                let persist_state = self.clone();
                tokio::spawn(async move {
                    // determinism-ok: async persistence, no simulation-visible state
                    if let Err(e) = persist_state.persist_wasm_invocation(&log_entry).await {
                        tracing::warn!(error = %e, "failed to persist WASM invocation log");
                    }
                });

                // Dispatch failure callback asynchronously (fire-and-forget)
                if let Some(ref on_failure) = integration.on_failure {
                    let state = self.clone();
                    let t = tenant.clone();
                    let et = entity_type.to_string();
                    let eid = entity_id.to_string();
                    let cb = on_failure.clone();
                    let int_name = integration.name.clone();
                    let mod_name = module_name.clone();
                    tokio::spawn(async move {
                        // determinism-ok: async callback delivery
                        let fail_params = serde_json::json!({
                            "error": format!("WASM module '{}' not found", mod_name),
                            "integration": int_name,
                        });
                        if let Err(e) = state
                            .dispatch_tenant_action(
                                &t,
                                &et,
                                &eid,
                                &cb,
                                fail_params,
                                &AgentContext::default(),
                            )
                            .await
                        {
                            tracing::error!(
                                callback = %cb,
                                error = %e,
                                "failed to dispatch WASM failure callback"
                            );
                        }
                    });
                }
                continue;
            }

            // Build authorization context for the WASM gate
            let authz_ctx = WasmAuthzContext {
                tenant: tenant.to_string(),
                module_name: module_name.clone(),
                agent_id: agent_ctx.agent_id.clone(),
                session_id: agent_ctx.session_id.clone(),
                entity_type: entity_type.to_string(),
                trigger_action: _action.to_string(),
            };

            // Build invocation context
            let ctx = WasmInvocationContext {
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                trigger_action: _action.to_string(),
                trigger_params: action_params.clone(),
                entity_state: serde_json::to_value(_entity_state).unwrap_or_default(),
                agent_id: agent_ctx.agent_id.clone(),
                session_id: agent_ctx.session_id.clone(),
                integration_config: match self.secrets_vault.as_ref() {
                    Some(vault) => {
                        resolve_secret_templates(&integration.config, vault, &tenant.to_string())
                    }
                    None => integration.config.clone(),
                },
            };

            // Look up module hash
            let module_hash = {
                let wasm_reg = self.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
                wasm_reg
                    .get_hash(tenant, module_name)
                    .map(|s| s.to_string())
            };
            let Some(hash) = module_hash else {
                // Already checked has_module above — should not happen
                continue;
            };

            tracing::info!(
                tenant = %tenant,
                entity_type,
                entity_id,
                integration = %integration.name,
                module = %module_name,
                hash = %hash,
                "invoking WASM integration module"
            );

            // Phase 3: Pre-filter secrets through authorization gate
            let tenant_secrets = self.get_authorized_wasm_secrets(tenant, &*gate, &authz_ctx);
            let inner: Arc<dyn WasmHost> = Arc::new(ProductionWasmHost::new(tenant_secrets));
            // Wrap with authorization decorator
            let host: Arc<dyn WasmHost> =
                Arc::new(AuthorizedWasmHost::new(inner, gate.clone(), authz_ctx));
            let limits = WasmResourceLimits::default();
            let engine = self.wasm_engine.clone();
            let state = self.clone();
            let t = tenant.clone();
            let et = entity_type.to_string();
            let eid = entity_id.to_string();
            let on_ok = integration.on_success.clone();
            let on_fail = integration.on_failure.clone();
            let int_name = integration.name.clone();
            let invocation_log = self.wasm_invocation_log.clone();
            let trigger_action = _action.to_string();
            let log_module_name = module_name.clone();

            tokio::spawn(async move {
                // determinism-ok: async WASM invocation
                match engine.invoke(&hash, &ctx, host, &limits).await {
                    Ok(result) if result.success => {
                        tracing::info!(
                            integration = %int_name,
                            callback_action = %result.callback_action,
                            duration_ms = result.duration_ms,
                            "WASM integration succeeded"
                        );
                        let log_entry = WasmInvocationEntry {
                            timestamp: sim_now().to_rfc3339(),
                            tenant: t.to_string(),
                            entity_type: et.clone(),
                            entity_id: eid.clone(),
                            module_name: log_module_name.clone(),
                            trigger_action: trigger_action.clone(),
                            callback_action: Some(result.callback_action.clone()),
                            success: true,
                            error: None,
                            duration_ms: result.duration_ms,
                            authz_denied: None,
                        };
                        if let Ok(mut log) = invocation_log.write() {
                            log.push(log_entry.clone());
                        }
                        // Persist to DB (fire-and-forget)
                        if let Err(e) = state.persist_wasm_invocation(&log_entry).await {
                            tracing::warn!(error = %e, "failed to persist WASM invocation log");
                        }
                        if let Some(cb) = on_ok {
                            let params = result.callback_params;
                            if let Err(e) = state
                                .dispatch_tenant_action(
                                    &t,
                                    &et,
                                    &eid,
                                    &cb,
                                    params,
                                    &AgentContext::default(),
                                )
                                .await
                            {
                                tracing::error!(callback = %cb, error = %e, "failed to dispatch WASM success callback");
                            }
                        }
                    }
                    Ok(result) => {
                        tracing::warn!(
                            integration = %int_name,
                            error = ?result.error,
                            duration_ms = result.duration_ms,
                            "WASM integration returned failure"
                        );
                        let error_str = result.error.clone().unwrap_or_default();
                        let is_authz_denied =
                            error_str.contains("authorization denied for http_call");

                        let log_entry = WasmInvocationEntry {
                            timestamp: sim_now().to_rfc3339(),
                            tenant: t.to_string(),
                            entity_type: et.clone(),
                            entity_id: eid.clone(),
                            module_name: log_module_name.clone(),
                            trigger_action: trigger_action.clone(),
                            callback_action: result.error.as_ref().and(on_fail.clone()),
                            success: false,
                            error: result.error.clone(),
                            duration_ms: result.duration_ms,
                            authz_denied: if is_authz_denied { Some(true) } else { None },
                        };
                        if let Ok(mut log) = invocation_log.write() {
                            log.push(log_entry.clone());
                        }
                        // Persist to DB (fire-and-forget)
                        if let Err(e) = state.persist_wasm_invocation(&log_entry).await {
                            tracing::warn!(error = %e, "failed to persist WASM invocation log");
                        }

                        // Surface authz denial as PendingDecision for human review.
                        let mut decision_id = None;
                        if is_authz_denied {
                            let pd = PendingDecision::from_denial(
                                t.as_str(),
                                "wasm-module",
                                "http_call",
                                "HttpEndpoint",
                                &int_name,
                                serde_json::json!({
                                    "entity_type": et,
                                    "entity_id": eid,
                                    "module": log_module_name,
                                    "trigger_action": trigger_action,
                                }),
                                &error_str,
                                Some(log_module_name.clone()),
                            );
                            decision_id = Some(pd.id.clone());
                            {
                                let mut pdlog = state.pending_decision_log.write().unwrap(); // ci-ok: infallible lock
                                if pdlog.push(pd.clone()) {
                                    let _ = state.pending_decision_tx.send(pd);
                                }
                            }

                            // Record trajectory for evolution engine.
                            let traj = TrajectoryEntry {
                                timestamp: sim_now().to_rfc3339(),
                                tenant: t.to_string(),
                                entity_type: et.clone(),
                                entity_id: eid.clone(),
                                action: trigger_action.clone(),
                                success: false,
                                from_status: None,
                                to_status: None,
                                error: Some(error_str.clone()),
                                agent_id: None,
                                session_id: None,
                                authz_denied: Some(true),
                                denied_resource: Some(int_name.clone()),
                                denied_module: Some(log_module_name.clone()),
                            };
                            if let Ok(mut tlog) = state.trajectory_log.write() {
                                tlog.push(traj);
                            }
                        }

                        if let Some(cb) = on_fail {
                            let mut params = serde_json::json!({
                                "error": error_str,
                                "integration": int_name,
                            });
                            // Include decision metadata for authz denials.
                            if is_authz_denied {
                                if let Some(ref did) = decision_id {
                                    params["decision_id"] = serde_json::json!(did);
                                }
                                params["authz_denied"] = serde_json::json!(true);
                            }
                            if let Err(e) = state
                                .dispatch_tenant_action(
                                    &t,
                                    &et,
                                    &eid,
                                    &cb,
                                    params,
                                    &AgentContext::default(),
                                )
                                .await
                            {
                                tracing::error!(callback = %cb, error = %e, "failed to dispatch WASM failure callback");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            integration = %int_name,
                            error = %e,
                            "WASM module invocation error"
                        );
                        let error_str = e.to_string();
                        let is_authz_denied =
                            error_str.contains("authorization denied for http_call");

                        let log_entry = WasmInvocationEntry {
                            timestamp: sim_now().to_rfc3339(),
                            tenant: t.to_string(),
                            entity_type: et.clone(),
                            entity_id: eid.clone(),
                            module_name: log_module_name.clone(),
                            trigger_action: trigger_action.clone(),
                            callback_action: on_fail.clone(),
                            success: false,
                            error: Some(error_str.clone()),
                            duration_ms: 0,
                            authz_denied: if is_authz_denied { Some(true) } else { None },
                        };
                        if let Ok(mut log) = invocation_log.write() {
                            log.push(log_entry.clone());
                        }
                        // Persist to DB (fire-and-forget)
                        if let Err(pe) = state.persist_wasm_invocation(&log_entry).await {
                            tracing::warn!(error = %pe, "failed to persist WASM invocation log");
                        }

                        // Surface authz denial as PendingDecision for human review.
                        let mut decision_id = None;
                        if is_authz_denied {
                            let pd = PendingDecision::from_denial(
                                t.as_str(),
                                "wasm-module",
                                "http_call",
                                "HttpEndpoint",
                                &int_name,
                                serde_json::json!({
                                    "entity_type": et,
                                    "entity_id": eid,
                                    "module": log_module_name,
                                    "trigger_action": trigger_action,
                                }),
                                &error_str,
                                Some(log_module_name.clone()),
                            );
                            decision_id = Some(pd.id.clone());
                            {
                                let mut pdlog = state.pending_decision_log.write().unwrap(); // ci-ok: infallible lock
                                if pdlog.push(pd.clone()) {
                                    let _ = state.pending_decision_tx.send(pd);
                                }
                            }
                        }

                        if let Some(cb) = on_fail {
                            let mut params = serde_json::json!({
                                "error": error_str,
                                "integration": int_name,
                            });
                            if is_authz_denied {
                                if let Some(ref did) = decision_id {
                                    params["decision_id"] = serde_json::json!(did);
                                }
                                params["authz_denied"] = serde_json::json!(true);
                            }
                            if let Err(ce) = state
                                .dispatch_tenant_action(
                                    &t,
                                    &et,
                                    &eid,
                                    &cb,
                                    params,
                                    &AgentContext::default(),
                                )
                                .await
                            {
                                tracing::error!(callback = %cb, error = %ce, "failed to dispatch WASM error callback");
                            }
                        }
                    }
                }
            });
        }
    }

    /// Get secrets filtered through the WASM authorization gate.
    ///
    /// Phase 3 defense-in-depth: only inject secrets that the gate authorizes
    /// into the `ProductionWasmHost`. Even if the decorator is somehow
    /// bypassed, unauthorized secrets aren't in memory.
    pub(crate) fn get_authorized_wasm_secrets(
        &self,
        tenant: &TenantId,
        gate: &dyn WasmAuthzGate,
        authz_ctx: &WasmAuthzContext,
    ) -> std::collections::BTreeMap<String, String> {
        let all_secrets = self
            .secrets_vault
            .as_ref()
            .map(|v| v.get_tenant_secrets(&tenant.to_string()))
            .unwrap_or_default();

        all_secrets
            .into_iter()
            .filter(|(key, _)| {
                gate.authorize_secret_access(key, authz_ctx)
                    == temper_wasm::WasmAuthzDecision::Allow
            })
            .collect()
    }

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
            agent_ctx,
            false,
        )
        .await
    }

    /// Dispatch with optional blocking integration await.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_tenant_action_ext(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
        await_integration: bool,
    ) -> Result<EntityResponse, String> {
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
        let action_params = params.clone();
        let Some(actor_ref) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) else {
            // Record a trajectory entry for the "no transition table" failure.
            let entry = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                action: action.to_string(),
                success: false,
                from_status: None,
                to_status: None,
                error: Some(format!(
                    "No transition table for tenant '{tenant}', entity type '{entity_type}'"
                )),
                agent_id: agent_ctx.agent_id.clone(),
                session_id: agent_ctx.session_id.clone(),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
            };
            if let Err(e) = self.persist_trajectory_entry(&entry).await {
                tracing::error!(error = %e, "failed to persist trajectory entry");
            } else if let Ok(mut log) = self.trajectory_log.write() {
                log.push(entry.clone());
            }
            return Err(format!(
                "No transition table for tenant '{tenant}', entity type '{entity_type}'"
            ));
        };

        let response = match actor_ref
            .ask::<EntityResponse>(
                EntityMsg::Action {
                    name: action.to_string(),
                    params,
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
                };
                if let Err(persist_err) = self.persist_trajectory_entry(&entry).await {
                    tracing::error!(error = %persist_err, "failed to persist trajectory entry");
                } else if let Ok(mut log) = self.trajectory_log.write() {
                    log.push(entry.clone());
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
        };
        // Best-effort persistence to event store.
        if let Err(e) = self.persist_trajectory_entry(&trajectory_entry).await {
            tracing::error!(error = %e, "failed to persist trajectory entry");
        }
        // Always push to in-memory log so /observe endpoints see it.
        if let Ok(mut log) = self.trajectory_log.write() {
            log.push(trajectory_entry.clone());
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
                        tenant,
                        entity_type,
                        entity_id,
                        action,
                        &response.custom_effects,
                        &response.state,
                        agent_ctx,
                        &action_params,
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

        Ok(response)
    }
}
