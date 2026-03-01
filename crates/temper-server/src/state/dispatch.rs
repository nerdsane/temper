//! Action dispatch and WASM integration methods for ServerState.

use std::sync::{Arc, Mutex};

use super::ServerState;
use super::pending_decisions::PendingDecision;
use super::trajectory::{TrajectoryEntry, TrajectorySource};
use super::wasm_invocation_log::WasmInvocationEntry;
use crate::dispatch::AgentContext;
use crate::entity_actor::{EntityMsg, EntityResponse, EntityState};
use crate::events::EntityStateChange;
use crate::secret_template::resolve_secret_templates;
use crate::wasm_authz_gate::PermissiveWasmAuthzGate;
use temper_observe::wide_event;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{
    AuthorizedWasmHost, ProductionWasmHost, WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate,
    WasmHost, WasmInvocationContext, WasmResourceLimits,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum WasmDispatchMode {
    Background,
    Inline,
}

#[derive(Clone, Copy)]
struct WasmEntityRef<'a> {
    tenant: &'a TenantId,
    entity_type: &'a str,
    entity_id: &'a str,
}

pub struct DispatchExtOptions<'a> {
    pub agent_ctx: &'a AgentContext,
    pub await_integration: bool,
}

#[derive(Clone, Default)]
struct HttpCallAuthzDenialTracker {
    denial_reason: Arc<Mutex<Option<String>>>,
}

impl HttpCallAuthzDenialTracker {
    fn record_denial(&self, reason: String) {
        if let Ok(mut slot) = self.denial_reason.lock()
            && slot.is_none()
        {
            *slot = Some(reason);
        }
    }

    fn take_denial(&self) -> Option<String> {
        self.denial_reason
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }
}

struct TrackingWasmAuthzGate {
    inner: Arc<dyn WasmAuthzGate>,
    tracker: HttpCallAuthzDenialTracker,
}

impl TrackingWasmAuthzGate {
    fn new(inner: Arc<dyn WasmAuthzGate>, tracker: HttpCallAuthzDenialTracker) -> Self {
        Self { inner, tracker }
    }
}

impl WasmAuthzGate for TrackingWasmAuthzGate {
    fn authorize_http_call(
        &self,
        domain: &str,
        method: &str,
        url: &str,
        ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        let decision = self.inner.authorize_http_call(domain, method, url, ctx);
        if let WasmAuthzDecision::Deny(reason) = &decision {
            self.tracker.record_denial(reason.clone());
        }
        decision
    }

    fn authorize_secret_access(
        &self,
        secret_key: &str,
        ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        self.inner.authorize_secret_access(secret_key, ctx)
    }
}

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
        let state = self.clone();
        let tenant = tenant.clone();
        let entity_type = entity_type.to_string();
        let entity_id = entity_id.to_string();
        let action = _action.to_string();
        let custom_effects = custom_effects.to_vec();
        let entity_state = _entity_state.clone();
        let agent_ctx = agent_ctx.clone();
        let action_params = action_params.clone();
        tokio::spawn(async move {
            // determinism-ok: async integration side-effects run outside simulation core
            if let Err(e) = state
                .dispatch_wasm_integrations_internal(
                    &tenant,
                    &entity_type,
                    &entity_id,
                    &action,
                    &custom_effects,
                    &entity_state,
                    &agent_ctx,
                    &action_params,
                    WasmDispatchMode::Background,
                )
                .await
            {
                tracing::error!(error = %e, "background WASM integration dispatch failed");
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn dispatch_wasm_integrations_internal(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        custom_effects: &[String],
        entity_state: &EntityState,
        agent_ctx: &AgentContext,
        action_params: &serde_json::Value,
        mode: WasmDispatchMode,
    ) -> Result<Option<EntityResponse>, String> {
        let integrations = {
            let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
            registry
                .get_spec(tenant, entity_type)
                .map(|spec| spec.integrations.clone())
                .unwrap_or_default()
        };
        let base_gate = self.wasm_authz_gate();
        let entity_ref = WasmEntityRef {
            tenant,
            entity_type,
            entity_id,
        };
        let mut last_response: Option<EntityResponse> = None;

        for effect_name in custom_effects {
            let integration = integrations
                .iter()
                .find(|ig| ig.integration_type == "wasm" && ig.trigger == *effect_name)
                .cloned();
            let Some(integration) = integration else {
                continue;
            };

            let Some(module_name) = integration.module.clone() else {
                tracing::warn!(
                    tenant = %tenant,
                    entity_type,
                    integration = %integration.name,
                    "WASM integration missing module name"
                );
                continue;
            };

            let module_hash = {
                let wasm_reg = self.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
                wasm_reg
                    .get_hash(tenant, &module_name)
                    .map(|s| s.to_string())
            };

            let Some(hash) = module_hash else {
                tracing::warn!(
                    tenant = %tenant,
                    entity_type,
                    module = %module_name,
                    "WASM module not found in registry"
                );
                let error_str = format!("WASM module '{}' not found", module_name);
                let log_entry = WasmInvocationEntry {
                    timestamp: sim_now().to_rfc3339(),
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    module_name: module_name.clone(),
                    trigger_action: action.to_string(),
                    callback_action: integration.on_failure.clone(),
                    success: false,
                    error: Some(error_str.clone()),
                    duration_ms: 0,
                    authz_denied: None,
                };
                if let Ok(mut log) = self.wasm_invocation_log.write() {
                    log.push(log_entry.clone());
                }
                let _ = self.persist_wasm_invocation(&log_entry).await;

                // Observability: emit WideEvent for module-not-found failure
                let wide = wide_event::from_wasm_invocation(
                    &module_name, action, entity_type, entity_id,
                    &tenant.to_string(), false, 0, Some(&error_str),
                );
                wide_event::emit_span(&wide);
                wide_event::emit_metrics(&wide);

                if let Some(ref cb) = integration.on_failure {
                    let params = serde_json::json!({
                        "error": error_str,
                        "integration": integration.name.clone(),
                    });
                    if let Some(resp) = self
                        .dispatch_wasm_callback(entity_ref, cb, params, agent_ctx, mode)
                        .await?
                    {
                        last_response = Some(resp);
                    }
                }
                continue;
            };

            let authz_ctx = WasmAuthzContext {
                tenant: tenant.to_string(),
                module_name: module_name.clone(),
                agent_id: agent_ctx.agent_id.clone(),
                session_id: agent_ctx.session_id.clone(),
                entity_type: entity_type.to_string(),
                trigger_action: action.to_string(),
            };
            let ctx = WasmInvocationContext {
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                trigger_action: action.to_string(),
                trigger_params: action_params.clone(),
                entity_state: serde_json::to_value(entity_state).unwrap_or_default(),
                agent_id: agent_ctx.agent_id.clone(),
                session_id: agent_ctx.session_id.clone(),
                integration_config: match self.secrets_vault.as_ref() {
                    Some(vault) => {
                        resolve_secret_templates(&integration.config, vault, &tenant.to_string())
                    }
                    None => integration.config.clone(),
                },
            };
            let denial_tracker = HttpCallAuthzDenialTracker::default();
            let gate: Arc<dyn WasmAuthzGate> = Arc::new(TrackingWasmAuthzGate::new(
                base_gate.clone(),
                denial_tracker.clone(),
            ));
            let tenant_secrets = self.get_authorized_wasm_secrets(tenant, &*gate, &authz_ctx);
            let inner: Arc<dyn WasmHost> = Arc::new(ProductionWasmHost::new(tenant_secrets));
            let host: Arc<dyn WasmHost> = Arc::new(AuthorizedWasmHost::new(inner, gate, authz_ctx));
            let limits = WasmResourceLimits::default();

            tracing::info!(
                tenant = %tenant,
                entity_type,
                entity_id,
                integration = %integration.name,
                module = %module_name,
                hash = %hash,
                "invoking WASM integration module"
            );

            match self.wasm_engine.invoke(&hash, &ctx, host, &limits).await {
                Ok(result) if result.success => {
                    if let Some(reason) = denial_tracker.take_denial() {
                        let error_str = format!("authorization denied for http_call: {reason}");
                        if let Some(resp) = self
                            .handle_wasm_failure(
                                entity_ref,
                                action,
                                &integration.name,
                                &module_name,
                                &integration.on_failure,
                                error_str,
                                result.duration_ms,
                                agent_ctx,
                                mode,
                            )
                            .await?
                        {
                            last_response = Some(resp);
                        }
                        continue;
                    }

                    let log_entry = WasmInvocationEntry {
                        timestamp: sim_now().to_rfc3339(),
                        tenant: tenant.to_string(),
                        entity_type: entity_type.to_string(),
                        entity_id: entity_id.to_string(),
                        module_name: module_name.clone(),
                        trigger_action: action.to_string(),
                        callback_action: Some(result.callback_action.clone()),
                        success: true,
                        error: None,
                        duration_ms: result.duration_ms,
                        authz_denied: None,
                    };
                    if let Ok(mut log) = self.wasm_invocation_log.write() {
                        log.push(log_entry.clone());
                    }
                    let _ = self.persist_wasm_invocation(&log_entry).await;

                    // Observability: emit WideEvent for successful WASM invocation
                    let wide = wide_event::from_wasm_invocation(
                        &module_name, action, entity_type, entity_id,
                        &tenant.to_string(), true, result.duration_ms * 1_000_000, None,
                    );
                    wide_event::emit_span(&wide);
                    wide_event::emit_metrics(&wide);

                    if let Some(ref cb) = integration.on_success
                        && let Some(resp) = self
                            .dispatch_wasm_callback(
                                entity_ref,
                                cb,
                                result.callback_params,
                                agent_ctx,
                                mode,
                            )
                            .await?
                    {
                        last_response = Some(resp);
                    }
                }
                Ok(result) => {
                    let mut error_str = result.error.unwrap_or_else(|| {
                        format!(
                            "WASM integration '{}' returned unsuccessful result",
                            integration.name
                        )
                    });
                    if let Some(reason) = denial_tracker.take_denial() {
                        error_str = format!("authorization denied for http_call: {reason}");
                    }

                    if let Some(resp) = self
                        .handle_wasm_failure(
                            entity_ref,
                            action,
                            &integration.name,
                            &module_name,
                            &integration.on_failure,
                            error_str,
                            result.duration_ms,
                            agent_ctx,
                            mode,
                        )
                        .await?
                    {
                        last_response = Some(resp);
                    }
                }
                Err(e) => {
                    let mut error_str = e.to_string();
                    if let Some(reason) = denial_tracker.take_denial()
                        && !error_str.contains("authorization denied for http_call")
                    {
                        error_str = format!("authorization denied for http_call: {reason}");
                    }

                    if let Some(resp) = self
                        .handle_wasm_failure(
                            entity_ref,
                            action,
                            &integration.name,
                            &module_name,
                            &integration.on_failure,
                            error_str,
                            0,
                            agent_ctx,
                            mode,
                        )
                        .await?
                    {
                        last_response = Some(resp);
                    }
                }
            }
        }

        Ok(last_response)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_wasm_failure(
        &self,
        entity_ref: WasmEntityRef<'_>,
        trigger_action: &str,
        integration_name: &str,
        module_name: &str,
        on_failure: &Option<String>,
        error_str: String,
        duration_ms: u64,
        agent_ctx: &AgentContext,
        mode: WasmDispatchMode,
    ) -> Result<Option<EntityResponse>, String> {
        let is_authz_denied = error_str.contains("authorization denied for http_call");
        let log_entry = WasmInvocationEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: entity_ref.tenant.to_string(),
            entity_type: entity_ref.entity_type.to_string(),
            entity_id: entity_ref.entity_id.to_string(),
            module_name: module_name.to_string(),
            trigger_action: trigger_action.to_string(),
            callback_action: on_failure.clone(),
            success: false,
            error: Some(error_str.clone()),
            duration_ms,
            authz_denied: if is_authz_denied { Some(true) } else { None },
        };
        if let Ok(mut log) = self.wasm_invocation_log.write() {
            log.push(log_entry.clone());
        }
        let _ = self.persist_wasm_invocation(&log_entry).await;

        // Observability: emit WideEvent for failed WASM invocation
        let wide = wide_event::from_wasm_invocation(
            module_name, trigger_action, entity_ref.entity_type, entity_ref.entity_id,
            &entity_ref.tenant.to_string(), false, duration_ms * 1_000_000, Some(&error_str),
        );
        wide_event::emit_span(&wide);
        wide_event::emit_metrics(&wide);

        let decision_id = if is_authz_denied {
            self.record_wasm_authz_denial(
                entity_ref,
                trigger_action,
                integration_name,
                module_name,
                &error_str,
            )
        } else {
            None
        };

        if let Some(cb) = on_failure {
            let mut params = serde_json::json!({
                "error": error_str,
                "integration": integration_name,
            });
            if let Some(ref did) = decision_id {
                params["decision_id"] = serde_json::json!(did);
                params["authz_denied"] = serde_json::json!(true);
            }
            return self
                .dispatch_wasm_callback(entity_ref, cb, params, agent_ctx, mode)
                .await;
        }

        Ok(None)
    }

    async fn dispatch_wasm_callback(
        &self,
        entity_ref: WasmEntityRef<'_>,
        callback_action: &str,
        callback_params: serde_json::Value,
        agent_ctx: &AgentContext,
        mode: WasmDispatchMode,
    ) -> Result<Option<EntityResponse>, String> {
        match mode {
            WasmDispatchMode::Inline => {
                let resp = self
                    .dispatch_tenant_action_core(
                        entity_ref.tenant,
                        entity_ref.entity_type,
                        entity_ref.entity_id,
                        callback_action,
                        callback_params,
                        agent_ctx,
                        false,
                    )
                    .await?;
                Ok(Some(resp))
            }
            WasmDispatchMode::Background => {
                if let Err(e) = self
                    .dispatch_tenant_action(
                        entity_ref.tenant,
                        entity_ref.entity_type,
                        entity_ref.entity_id,
                        callback_action,
                        callback_params,
                        &AgentContext::default(),
                    )
                    .await
                {
                    tracing::error!(
                        callback = %callback_action,
                        error = %e,
                        "failed to dispatch WASM callback"
                    );
                }
                Ok(None)
            }
        }
    }

    fn record_wasm_authz_denial(
        &self,
        entity_ref: WasmEntityRef<'_>,
        trigger_action: &str,
        integration_name: &str,
        module_name: &str,
        error_str: &str,
    ) -> Option<String> {
        let pd = PendingDecision::from_denial(
            entity_ref.tenant.as_str(),
            "wasm-module",
            "http_call",
            "HttpEndpoint",
            integration_name,
            serde_json::json!({
                "entity_type": entity_ref.entity_type,
                "entity_id": entity_ref.entity_id,
                "module": module_name,
                "trigger_action": trigger_action,
            }),
            error_str,
            Some(module_name.to_string()),
        );
        let decision_id = pd.id.clone();
        if let Ok(mut pdlog) = self.pending_decision_log.write()
            && pdlog.push(pd.clone())
        {
            let _ = self.pending_decision_tx.send(pd);
        }

        let traj = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: entity_ref.tenant.to_string(),
            entity_type: entity_ref.entity_type.to_string(),
            entity_id: entity_ref.entity_id.to_string(),
            action: trigger_action.to_string(),
            success: false,
            from_status: None,
            to_status: None,
            error: Some(error_str.to_string()),
            agent_id: None,
            session_id: None,
            authz_denied: Some(true),
            denied_resource: Some(integration_name.to_string()),
            denied_module: Some(module_name.to_string()),
            source: Some(TrajectorySource::Authz),
            spec_governed: None,
        };
        if let Ok(mut tlog) = self.trajectory_log.write() {
            tlog.push(traj);
        }

        Some(decision_id)
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
            if let Ok(mut log) = self.trajectory_log.write() {
                log.push(entry);
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
            source: Some(TrajectorySource::Entity),
            spec_governed: None,
        };
        // Best-effort persistence to event store.
        if let Err(e) = self.persist_trajectory_entry(&trajectory_entry).await {
            tracing::error!(error = %e, "failed to persist trajectory entry");
        }
        // Always push to in-memory log so /observe endpoints see it.
        if let Ok(mut log) = self.trajectory_log.write() {
            log.push(trajectory_entry.clone());
        }
        // Push successful transitions to the dedicated success log (separate budget).
        if trajectory_entry.success && let Ok(mut log) = self.success_trajectory_log.write() {
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
                        super::dispatch_blocking::BlockingWasmDispatch {
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

    /// Pre-resolve cross-entity state guards for an action.
    ///
    /// Reads the TransitionTable, walks rules for the given action, and for each
    /// `CrossEntityStateIn` guard, resolves the target entity's status and compares
    /// against the required statuses.
    async fn resolve_cross_entity_guards(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> std::collections::BTreeMap<String, bool> {
        use crate::entity_actor::effects::MAX_CROSS_ENTITY_LOOKUPS;

        let mut result = std::collections::BTreeMap::new();

        // Get the transition table to find cross-entity guards
        let cross_guards: Vec<(String, String, Vec<String>)> = {
            let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
            let Some(spec) = registry.get_spec(tenant, entity_type) else {
                return result;
            };
            let table = spec.table();

            // Collect CrossEntityStateIn guards from rules matching this action
            let mut guards = Vec::new();
            for rule in &table.rules {
                if rule.name == action {
                    Self::collect_cross_guards(&rule.guard, &mut guards);
                }
            }
            guards
        };

        if cross_guards.is_empty() {
            return result;
        }

        // Get current entity fields to resolve target entity IDs
        let current_fields = match self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            Ok(resp) => resp.state.fields,
            Err(_) => return result,
        };

        // Resolve each cross-entity guard (budget-limited)
        let mut lookup_count = 0;
        for (target_type, id_source, required_statuses) in &cross_guards {
            if lookup_count >= MAX_CROSS_ENTITY_LOOKUPS {
                tracing::warn!(
                    entity_type,
                    entity_id,
                    "cross-entity lookup budget exhausted ({})",
                    MAX_CROSS_ENTITY_LOOKUPS
                );
                break;
            }

            let target_id = current_fields
                .get(id_source)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if target_id.is_empty() {
                let key = format!("__xref:{}:{}", target_type, id_source);
                result.insert(key, false);
                continue;
            }

            lookup_count += 1;
            let key = format!("__xref:{}:{}", target_type, id_source);
            if let Some(status) = self
                .resolve_entity_status(tenant, target_type, target_id)
                .await
            {
                let matched = required_statuses.iter().any(|s| s == &status);
                result.insert(key, matched);
            } else {
                result.insert(key, false);
            }
        }

        result
    }

    /// Recursively collect CrossEntityStateIn guards from a guard tree.
    fn collect_cross_guards(
        guard: &temper_jit::table::Guard,
        out: &mut Vec<(String, String, Vec<String>)>,
    ) {
        use temper_jit::table::Guard;
        match guard {
            Guard::CrossEntityStateIn {
                entity_type,
                entity_id_source,
                required_status,
            } => {
                out.push((
                    entity_type.clone(),
                    entity_id_source.clone(),
                    required_status.clone(),
                ));
            }
            Guard::And(guards) => {
                for g in guards {
                    Self::collect_cross_guards(g, out);
                }
            }
            _ => {}
        }
    }

    /// Dispatch entity spawn requests post-transition.
    ///
    /// This is a **sync** method (like `dispatch_scheduled_actions`) so that
    /// `tokio::spawn` inside it does not cause async recursion.
    /// Creates child entities and optionally dispatches initial actions.
    fn dispatch_spawn_requests(
        &self,
        tenant: &TenantId,
        parent_type: &str,
        parent_id: &str,
        spawn_requests: &[crate::entity_actor::effects::SpawnRequest],
        agent_ctx: &AgentContext,
    ) {
        use crate::entity_actor::effects::MAX_SPAWNS_PER_TRANSITION;

        for (spawn_count, req) in spawn_requests.iter().enumerate() {
            if spawn_count >= MAX_SPAWNS_PER_TRANSITION {
                tracing::warn!(
                    parent_type,
                    parent_id,
                    "spawn budget exhausted ({})",
                    MAX_SPAWNS_PER_TRANSITION
                );
                break;
            }

            let state = self.clone();
            let t = tenant.clone();
            let parent_t = parent_type.to_string();
            let parent_i = parent_id.to_string();
            let child_type = req.entity_type.clone();
            let child_id = req.entity_id.clone();
            let initial_action = req.initial_action.clone();
            let agent = agent_ctx.clone();

            tokio::spawn(async move {
                // determinism-ok: spawn dispatch is a background side-effect
                let initial_fields = serde_json::json!({
                    "parent_type": parent_t,
                    "parent_id": parent_i,
                });

                match state
                    .get_or_create_tenant_entity(&t, &child_type, &child_id, initial_fields)
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            parent_type = %parent_t,
                            parent_id = %parent_i,
                            child_type = %child_type,
                            child_id = %child_id,
                            "spawned child entity"
                        );

                        if let Some(action) = initial_action
                            && let Err(e) = state
                                .dispatch_tenant_action(
                                    &t,
                                    &child_type,
                                    &child_id,
                                    &action,
                                    serde_json::json!({}),
                                    &agent,
                                )
                                .await
                        {
                            tracing::error!(
                                child_type = %child_type,
                                child_id = %child_id,
                                action = %action,
                                error = %e,
                                "failed to dispatch initial action on spawned entity"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            child_type = %child_type,
                            child_id = %child_id,
                            error = %e,
                            "failed to spawn child entity"
                        );
                    }
                }
            });
        }
    }
}
