use tracing::instrument;

use crate::adapters::{AdapterAgentContext, AdapterContext, AdapterResult};
use crate::entity_actor::{EntityResponse, EntityState};
use crate::request_context::AgentContext;
use crate::secrets::template::resolve_secret_templates;
use temper_runtime::tenant::TenantId;

use super::{WasmDispatchMode, WasmDispatchRequest, WasmEntityRef};

struct AdapterDispatchCtx<'a> {
    entity_ref: WasmEntityRef<'a>,
    action: &'a str,
    agent_ctx: &'a AgentContext,
    mode: WasmDispatchMode,
}

pub(crate) struct AdapterDispatchInput<'a> {
    pub(crate) tenant: &'a TenantId,
    pub(crate) entity_type: &'a str,
    pub(crate) entity_id: &'a str,
    pub(crate) action: &'a str,
    pub(crate) custom_effects: &'a [String],
    pub(crate) entity_state: &'a EntityState,
    pub(crate) agent_ctx: &'a AgentContext,
    pub(crate) action_params: &'a serde_json::Value,
}

impl crate::state::ServerState {
    /// Dispatch native adapter integrations for custom effects in background mode.
    pub(crate) fn dispatch_adapter_integrations(&self, input: AdapterDispatchInput<'_>) {
        let state = self.clone();
        let tenant = input.tenant.clone();
        let entity_type = input.entity_type.to_string();
        let entity_id = input.entity_id.to_string();
        let action = input.action.to_string();
        let custom_effects = input.custom_effects.to_vec();
        let entity_state = input.entity_state.clone();
        let agent_ctx = input.agent_ctx.clone();
        let action_params = input.action_params.clone();

        tokio::spawn(async move {
            // determinism-ok: async integration side-effects run outside simulation core
            let req = WasmDispatchRequest {
                tenant: &tenant,
                entity_type: &entity_type,
                entity_id: &entity_id,
                action: &action,
                custom_effects: &custom_effects,
                entity_state: &entity_state,
                agent_ctx: &agent_ctx,
                action_params: &action_params,
                mode: WasmDispatchMode::Background,
            };
            if let Err(e) = state.dispatch_adapter_integrations_internal(&req).await {
                tracing::error!(error = %e, "background adapter integration dispatch failed");
            }
        });
    }

    /// Dispatch adapter integrations in either inline or background mode.
    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_adapter_integrations_internal", tenant = %req.tenant, entity_type = req.entity_type, entity_id = req.entity_id, action_name = req.action))]
    pub(crate) async fn dispatch_adapter_integrations_internal(
        &self,
        req: &WasmDispatchRequest<'_>,
    ) -> Result<Option<EntityResponse>, String> {
        let integrations = {
            let registry = self
                .registry
                .read()
                .map_err(|e| format!("registry lock poisoned: {e}"))?;
            registry
                .get_spec(req.tenant, req.entity_type)
                .map(|spec| spec.integrations.clone())
                .unwrap_or_default()
        };

        let ctx = AdapterDispatchCtx {
            entity_ref: WasmEntityRef {
                tenant: req.tenant,
                entity_type: req.entity_type,
                entity_id: req.entity_id,
            },
            action: req.action,
            agent_ctx: req.agent_ctx,
            mode: req.mode,
        };

        let mut last_response: Option<EntityResponse> = None;

        for effect_name in req.custom_effects {
            let integration = integrations
                .iter()
                .find(|ig| ig.integration_type == "adapter" && ig.trigger == *effect_name)
                .cloned();
            let Some(integration) = integration else {
                continue;
            };

            if let Some(resp) = self
                .dispatch_single_adapter_integration(
                    &ctx,
                    &integration,
                    req.entity_state,
                    req.action_params,
                )
                .await?
            {
                last_response = Some(resp);
            }
        }

        Ok(last_response)
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_single_adapter_integration", integration = %integration.name))]
    async fn dispatch_single_adapter_integration(
        &self,
        ctx: &AdapterDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        entity_state: &EntityState,
        action_params: &serde_json::Value,
    ) -> Result<Option<EntityResponse>, String> {
        let adapter_type = entity_state
            .fields
            .get("adapter_type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| integration.config.get("adapter").cloned())
            .or_else(|| integration.config.get("adapter_type").cloned())
            .ok_or_else(|| {
                format!(
                    "adapter integration '{}' is missing required config key 'adapter'",
                    integration.name
                )
            })?;

        let Some(adapter) = self.adapter_registry.get(&adapter_type) else {
            return self
                .handle_adapter_failure(
                    ctx,
                    integration,
                    format!("adapter '{adapter_type}' not found in registry"),
                    0,
                )
                .await;
        };

        let tenant = ctx.entity_ref.tenant.to_string();
        let integration_config = match self.secrets_vault.as_ref() {
            Some(vault) => resolve_secret_templates(&integration.config, vault, &tenant),
            None => integration.config.clone(),
        };
        let secrets = self
            .secrets_vault
            .as_ref()
            .map(|vault| vault.get_tenant_secrets(&tenant))
            .unwrap_or_default();

        let adapter_ctx = AdapterContext {
            tenant,
            entity_type: ctx.entity_ref.entity_type.to_string(),
            entity_id: ctx.entity_ref.entity_id.to_string(),
            trigger_action: ctx.action.to_string(),
            trigger_params: action_params.clone(),
            entity_state: serde_json::to_value(entity_state).unwrap_or_default(),
            integration_config,
            agent_ctx: AdapterAgentContext {
                agent_id: ctx.agent_ctx.agent_id.clone(),
                session_id: ctx.agent_ctx.session_id.clone(),
                agent_type: ctx.agent_ctx.agent_type.clone(),
            },
            secrets,
        };

        let result = match adapter.execute(adapter_ctx).await {
            Ok(result) => result,
            Err(e) => {
                return self
                    .handle_adapter_failure(ctx, integration, e.to_string(), 0)
                    .await;
            }
        };

        if result.success {
            let callback_action = integration
                .on_success
                .clone()
                .or_else(|| result.callback_action.clone());
            let Some(callback_action) = callback_action else {
                return Ok(None);
            };
            let callback_params = normalize_success_params(result);
            return self
                .dispatch_adapter_callback(
                    ctx.entity_ref,
                    &callback_action,
                    callback_params,
                    ctx.agent_ctx,
                    ctx.mode,
                )
                .await;
        }

        let error = result
            .error
            .clone()
            .unwrap_or_else(|| "adapter returned unsuccessful result".to_string());
        self.handle_adapter_failure(ctx, integration, error, result.duration_ms)
            .await
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.handle_adapter_failure", integration = %integration.name))]
    async fn handle_adapter_failure(
        &self,
        ctx: &AdapterDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        error: String,
        duration_ms: u64,
    ) -> Result<Option<EntityResponse>, String> {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_type = ctx.entity_ref.entity_type,
            entity_id = ctx.entity_ref.entity_id,
            integration = %integration.name,
            error = %error,
            "adapter integration failed"
        );

        let Some(callback_action) = integration.on_failure.clone() else {
            return Ok(None);
        };

        let params = serde_json::json!({
            "error": error,
            "error_message": error,
            "integration": integration.name,
            "duration_ms": duration_ms,
        });

        self.dispatch_adapter_callback(
            ctx.entity_ref,
            &callback_action,
            params,
            ctx.agent_ctx,
            ctx.mode,
        )
        .await
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_adapter_callback", callback_action))]
    async fn dispatch_adapter_callback(
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
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Some(resp))
            }
            WasmDispatchMode::Background => {
                self.dispatch_tenant_action(
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                    callback_action,
                    callback_params,
                    &AgentContext::system(),
                )
                .await
                .map_err(|e| {
                    let msg =
                        format!("failed to dispatch adapter callback '{callback_action}': {e}");
                    tracing::error!(callback = %callback_action, error = %e, "{msg}");
                    msg
                })?;
                Ok(None)
            }
        }
    }
}

fn normalize_success_params(result: AdapterResult) -> serde_json::Value {
    let mut callback_params = result.callback_params;
    match callback_params {
        serde_json::Value::Object(ref mut obj) => {
            obj.entry("duration_ms".to_string())
                .or_insert(serde_json::json!(result.duration_ms));
            if let Some(error) = result.error {
                obj.entry("adapter_error".to_string())
                    .or_insert(serde_json::json!(error));
            }
            callback_params
        }
        _ => serde_json::json!({
            "result": callback_params,
            "duration_ms": result.duration_ms,
        }),
    }
}
