use std::sync::Arc;

use tracing::instrument;

use crate::dispatch::AgentContext;
use crate::entity_actor::{EntityResponse, EntityState};
use crate::secret_template::resolve_secret_templates;
use crate::state::pending_decisions::PendingDecision;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::state::wasm_invocation_log::WasmInvocationEntry;
use temper_observe::wide_event;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{
    AuthorizedWasmHost, ProductionWasmHost, WasmAuthzContext, WasmAuthzGate, WasmHost,
    WasmInvocationContext, WasmResourceLimits,
};

use super::{HttpCallAuthzDenialTracker, TrackingWasmAuthzGate, WasmDispatchMode, WasmEntityRef};

impl crate::state::ServerState {
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_wasm_integrations_internal", tenant = %tenant, entity_type, entity_id, action_name = action))]
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
                // WASM invocations are persisted to Turso directly.
                let _ = self.persist_wasm_invocation(&log_entry).await;

                // Observability: emit WideEvent for module-not-found failure
                let wide = wide_event::from_wasm_invocation(
                    &module_name,
                    action,
                    entity_type,
                    entity_id,
                    &tenant.to_string(),
                    false,
                    0,
                    Some(&error_str),
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
                    let _ = self.persist_wasm_invocation(&log_entry).await;

                    // Observability: emit WideEvent for successful WASM invocation
                    let wide = wide_event::from_wasm_invocation(
                        &module_name,
                        action,
                        entity_type,
                        entity_id,
                        &tenant.to_string(),
                        true,
                        result.duration_ms * 1_000_000,
                        None,
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
    #[instrument(skip_all, fields(otel.name = "dispatch.handle_wasm_failure", trigger_action, integration_name, module_name))]
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
        // WASM invocations are persisted to Turso directly.
        let _ = self.persist_wasm_invocation(&log_entry).await;

        // Observability: emit WideEvent for failed WASM invocation
        let wide = wide_event::from_wasm_invocation(
            module_name,
            trigger_action,
            entity_ref.entity_type,
            entity_ref.entity_id,
            &entity_ref.tenant.to_string(),
            false,
            duration_ms * 1_000_000,
            Some(&error_str),
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

    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_wasm_callback", callback_action))]
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
        let _ = self.pending_decision_tx.send(pd.clone());
        {
            let state_c = self.clone();
            tokio::spawn(async move {
                // determinism-ok: background persist for sync WASM authz path
                if let Err(e) = state_c.persist_pending_decision(&pd).await {
                    tracing::error!(error = %e, "failed to persist WASM authz decision");
                }
            });
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
        {
            let state_c = self.clone();
            tokio::spawn(async move {
                // determinism-ok: background persist for sync WASM authz path
                if let Err(e) = state_c.persist_trajectory_entry(&traj).await {
                    tracing::error!(error = %e, "failed to persist WASM authz trajectory");
                }
            });
        }

        Some(decision_id)
    }
}
