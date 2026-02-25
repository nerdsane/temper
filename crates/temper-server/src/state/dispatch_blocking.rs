//! Blocking (inline-await) WASM integration dispatch.
//!
//! When `?await_integration=true` is set, the HTTP handler awaits WASM
//! invocations inline instead of spawning fire-and-forget tasks. This
//! lets agents do "action + integration" in a single request.

use std::sync::Arc;

use super::ServerState;
use super::wasm_invocation_log::WasmInvocationEntry;
use crate::dispatch::AgentContext;
use crate::entity_actor::{EntityResponse, EntityState};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{ProductionWasmHost, WasmHost, WasmInvocationContext, WasmResourceLimits};

impl ServerState {
    /// Dispatch WASM integrations inline (blocking), returning the final
    /// post-callback `EntityResponse` if any integration matched.
    ///
    /// Unlike `dispatch_wasm_integrations` which spawns fire-and-forget
    /// tasks, this method awaits each WASM invocation and callback
    /// dispatch inline. Returns `Ok(None)` if no integrations matched.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_wasm_integrations_blocking<'a>(
        &'a self,
        tenant: &'a TenantId,
        entity_type: &'a str,
        entity_id: &'a str,
        action: &'a str,
        custom_effects: &'a [String],
        entity_state: &'a EntityState,
        agent_ctx: &'a AgentContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<EntityResponse>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Look up integrations for this entity type.
            let integrations = {
                let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
                registry
                    .get_spec(tenant, entity_type)
                    .map(|spec| spec.integrations.clone())
                    .unwrap_or_default()
            };

            let mut last_response: Option<EntityResponse> = None;

            for effect_name in custom_effects {
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
                        "WASM integration missing module name (blocking)"
                    );
                    continue;
                };

                // Look up module hash.
                let module_hash = {
                    let wasm_reg = self.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
                    wasm_reg
                        .get_hash(tenant, module_name)
                        .map(|s| s.to_string())
                };

                let Some(hash) = module_hash else {
                    tracing::warn!(
                        tenant = %tenant,
                        entity_type,
                        module = %module_name,
                        "WASM module not found in registry (blocking)"
                    );
                    // Record invocation log entry for module-not-found.
                    let log_entry = WasmInvocationEntry {
                        timestamp: sim_now().to_rfc3339(),
                        tenant: tenant.to_string(),
                        entity_type: entity_type.to_string(),
                        entity_id: entity_id.to_string(),
                        module_name: module_name.clone(),
                        trigger_action: action.to_string(),
                        callback_action: integration.on_failure.clone(),
                        success: false,
                        error: Some(format!("WASM module '{}' not found", module_name)),
                        duration_ms: 0,
                    };
                    if let Ok(mut log) = self.wasm_invocation_log.write() {
                        log.push(log_entry.clone());
                    }
                    let _ = self.persist_wasm_invocation(&log_entry).await;

                    // Dispatch failure callback inline.
                    if let Some(ref on_failure) = integration.on_failure {
                        let fail_params = serde_json::json!({
                            "error": format!("WASM module '{}' not found", module_name),
                            "integration": integration.name,
                        });
                        match self
                            .dispatch_tenant_action_core(
                                tenant,
                                entity_type,
                                entity_id,
                                on_failure,
                                fail_params,
                                agent_ctx,
                                false,
                            )
                            .await
                        {
                            Ok(resp) => last_response = Some(resp),
                            Err(e) => {
                                tracing::error!(callback = %on_failure, error = %e, "failed to dispatch WASM failure callback (blocking)");
                                return Err(e);
                            }
                        }
                    }
                    continue;
                };

                // Build invocation context.
                let ctx = WasmInvocationContext {
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    trigger_action: action.to_string(),
                    trigger_params: serde_json::Value::Null,
                    entity_state: serde_json::to_value(entity_state).unwrap_or_default(),
                    agent_id: agent_ctx.agent_id.clone(),
                    session_id: agent_ctx.session_id.clone(),
                };

                let tenant_secrets = self
                    .secrets_vault
                    .as_ref()
                    .map(|v| v.get_tenant_secrets(&tenant.to_string()))
                    .unwrap_or_default();
                let host: Arc<dyn WasmHost> = Arc::new(ProductionWasmHost::new(tenant_secrets));
                let limits = WasmResourceLimits::default();

                tracing::info!(
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    integration = %integration.name,
                    module = %module_name,
                    hash = %hash,
                    "invoking WASM integration module (blocking)"
                );

                // Await the invocation inline — no tokio::spawn.
                match self.wasm_engine.invoke(&hash, &ctx, host, &limits).await {
                    Ok(result) if result.success => {
                        tracing::info!(
                            integration = %integration.name,
                            callback_action = %result.callback_action,
                            duration_ms = result.duration_ms,
                            "WASM integration succeeded (blocking)"
                        );
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
                        };
                        if let Ok(mut log) = self.wasm_invocation_log.write() {
                            log.push(log_entry.clone());
                        }
                        let _ = self.persist_wasm_invocation(&log_entry).await;

                        if let Some(cb) = &integration.on_success {
                            match self
                                .dispatch_tenant_action_core(
                                    tenant,
                                    entity_type,
                                    entity_id,
                                    cb,
                                    result.callback_params,
                                    agent_ctx,
                                    false,
                                )
                                .await
                            {
                                Ok(resp) => last_response = Some(resp),
                                Err(e) => {
                                    tracing::error!(callback = %cb, error = %e, "failed to dispatch WASM success callback (blocking)");
                                    return Err(e);
                                }
                            }
                        }
                    }
                    Ok(result) => {
                        tracing::warn!(
                            integration = %integration.name,
                            error = ?result.error,
                            duration_ms = result.duration_ms,
                            "WASM integration returned failure (blocking)"
                        );
                        let log_entry = WasmInvocationEntry {
                            timestamp: sim_now().to_rfc3339(),
                            tenant: tenant.to_string(),
                            entity_type: entity_type.to_string(),
                            entity_id: entity_id.to_string(),
                            module_name: module_name.clone(),
                            trigger_action: action.to_string(),
                            callback_action: result
                                .error
                                .as_ref()
                                .and(integration.on_failure.clone()),
                            success: false,
                            error: result.error.clone(),
                            duration_ms: result.duration_ms,
                        };
                        if let Ok(mut log) = self.wasm_invocation_log.write() {
                            log.push(log_entry.clone());
                        }
                        let _ = self.persist_wasm_invocation(&log_entry).await;

                        if let Some(ref cb) = integration.on_failure {
                            let params = serde_json::json!({
                                "error": result.error.unwrap_or_default(),
                                "integration": integration.name,
                            });
                            match self
                                .dispatch_tenant_action_core(
                                    tenant,
                                    entity_type,
                                    entity_id,
                                    cb,
                                    params,
                                    agent_ctx,
                                    false,
                                )
                                .await
                            {
                                Ok(resp) => last_response = Some(resp),
                                Err(e) => {
                                    tracing::error!(callback = %cb, error = %e, "failed to dispatch WASM failure callback (blocking)");
                                    return Err(e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            integration = %integration.name,
                            error = %e,
                            "WASM module invocation error (blocking)"
                        );
                        let log_entry = WasmInvocationEntry {
                            timestamp: sim_now().to_rfc3339(),
                            tenant: tenant.to_string(),
                            entity_type: entity_type.to_string(),
                            entity_id: entity_id.to_string(),
                            module_name: module_name.clone(),
                            trigger_action: action.to_string(),
                            callback_action: integration.on_failure.clone(),
                            success: false,
                            error: Some(e.to_string()),
                            duration_ms: 0,
                        };
                        if let Ok(mut log) = self.wasm_invocation_log.write() {
                            log.push(log_entry.clone());
                        }
                        let _ = self.persist_wasm_invocation(&log_entry).await;

                        if let Some(ref cb) = integration.on_failure {
                            let params = serde_json::json!({
                                "error": e.to_string(),
                                "integration": integration.name,
                            });
                            match self
                                .dispatch_tenant_action_core(
                                    tenant,
                                    entity_type,
                                    entity_id,
                                    cb,
                                    params,
                                    agent_ctx,
                                    false,
                                )
                                .await
                            {
                                Ok(resp) => last_response = Some(resp),
                                Err(e) => {
                                    tracing::error!(callback = %cb, error = %e, "failed to dispatch WASM error callback (blocking)");
                                    return Err(e);
                                }
                            }
                        }
                    }
                }
            }

            Ok(last_response)
        })
    }
}
