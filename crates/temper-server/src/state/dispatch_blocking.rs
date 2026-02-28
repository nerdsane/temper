//! Blocking (inline-await) WASM integration dispatch.
//!
//! When `?await_integration=true` is set, the HTTP handler awaits WASM
//! invocations inline instead of spawning fire-and-forget tasks. This
//! lets agents do "action + integration" in a single request.

use std::sync::Arc;

use super::ServerState;
use super::pending_decisions::PendingDecision;
use super::trajectory::{TrajectoryEntry, TrajectorySource};
use super::wasm_invocation_log::WasmInvocationEntry;
use crate::dispatch::AgentContext;
use crate::entity_actor::{EntityResponse, EntityState};
use crate::secret_template::resolve_secret_templates;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{
    AuthorizedWasmHost, ProductionWasmHost, WasmAuthzContext, WasmHost, WasmInvocationContext,
    WasmResourceLimits,
};

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
        action_params: &'a serde_json::Value,
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

            let gate = self.wasm_authz_gate();
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
                        authz_denied: None,
                    };
                    if let Ok(mut log) = self.wasm_invocation_log.write() {
                        log.push(log_entry.clone());
                    }
                    let _ = self.persist_wasm_invocation(&log_entry).await;

                    // Dispatch failure callback inline.
                    if let Some(ref on_failure) = integration.on_failure {
                        let fail_params = serde_json::json!({
                            "error": format!("WASM module '{}' not found", module_name),
                            "integration": integration.name.clone(),
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

                // Build authorization context for the WASM gate
                let authz_ctx = WasmAuthzContext {
                    tenant: tenant.to_string(),
                    module_name: module_name.clone(),
                    agent_id: agent_ctx.agent_id.clone(),
                    session_id: agent_ctx.session_id.clone(),
                    entity_type: entity_type.to_string(),
                    trigger_action: action.to_string(),
                };

                // Build invocation context.
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
                        Some(vault) => resolve_secret_templates(
                            &integration.config,
                            vault,
                            &tenant.to_string(),
                        ),
                        None => integration.config.clone(),
                    },
                };

                // Phase 3: Pre-filter secrets through authorization gate
                let tenant_secrets = self.get_authorized_wasm_secrets(tenant, &*gate, &authz_ctx);
                let inner: Arc<dyn WasmHost> = Arc::new(ProductionWasmHost::new(tenant_secrets));
                // Wrap with authorization decorator
                let host: Arc<dyn WasmHost> =
                    Arc::new(AuthorizedWasmHost::new(inner, gate.clone(), authz_ctx));
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
                            authz_denied: None,
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
                        let error_str = result.error.clone().unwrap_or_default();
                        let is_authz_denied =
                            error_str.contains("authorization denied for http_call");
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
                            authz_denied: if is_authz_denied { Some(true) } else { None },
                        };
                        if let Ok(mut log) = self.wasm_invocation_log.write() {
                            log.push(log_entry.clone());
                        }
                        let _ = self.persist_wasm_invocation(&log_entry).await;

                        // Surface authz denials as pending decisions + trajectory entries.
                        let mut decision_id = None;
                        if is_authz_denied {
                            let pd = PendingDecision::from_denial(
                                tenant.as_str(),
                                "wasm-module",
                                "http_call",
                                "HttpEndpoint",
                                &integration.name,
                                serde_json::json!({
                                    "entity_type": entity_type,
                                    "entity_id": entity_id,
                                    "module": module_name,
                                    "trigger_action": action,
                                }),
                                &error_str,
                                Some(module_name.clone()),
                            );
                            decision_id = Some(pd.id.clone());
                            {
                                let mut pdlog = self.pending_decision_log.write().unwrap(); // ci-ok: infallible lock
                                if pdlog.push(pd.clone()) {
                                    let _ = self.pending_decision_tx.send(pd);
                                }
                            }

                            let traj = TrajectoryEntry {
                                timestamp: sim_now().to_rfc3339(),
                                tenant: tenant.to_string(),
                                entity_type: entity_type.to_string(),
                                entity_id: entity_id.to_string(),
                                action: action.to_string(),
                                success: false,
                                from_status: None,
                                to_status: None,
                                error: Some(error_str.clone()),
                                agent_id: None,
                                session_id: None,
                                authz_denied: Some(true),
                                denied_resource: Some(integration.name.clone()),
                                denied_module: Some(module_name.clone()),
                                source: Some(TrajectorySource::Authz),
                            };
                            if let Ok(mut tlog) = self.trajectory_log.write() {
                                tlog.push(traj);
                            }
                        }

                        if let Some(ref cb) = integration.on_failure {
                            let mut params = serde_json::json!({
                                "error": error_str,
                                "integration": integration.name.clone(),
                            });
                            if is_authz_denied {
                                if let Some(ref did) = decision_id {
                                    params["decision_id"] = serde_json::json!(did);
                                }
                                params["authz_denied"] = serde_json::json!(true);
                            }
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
                        let error_str = e.to_string();
                        let is_authz_denied =
                            error_str.contains("authorization denied for http_call");
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
                            authz_denied: if is_authz_denied { Some(true) } else { None },
                        };
                        if let Ok(mut log) = self.wasm_invocation_log.write() {
                            log.push(log_entry.clone());
                        }
                        let _ = self.persist_wasm_invocation(&log_entry).await;

                        // Surface authz denials as pending decisions.
                        let mut decision_id = None;
                        if is_authz_denied {
                            let pd = PendingDecision::from_denial(
                                tenant.as_str(),
                                "wasm-module",
                                "http_call",
                                "HttpEndpoint",
                                &integration.name,
                                serde_json::json!({
                                    "entity_type": entity_type,
                                    "entity_id": entity_id,
                                    "module": module_name,
                                    "trigger_action": action,
                                }),
                                &error_str,
                                Some(module_name.clone()),
                            );
                            decision_id = Some(pd.id.clone());
                            {
                                let mut pdlog = self.pending_decision_log.write().unwrap(); // ci-ok: infallible lock
                                if pdlog.push(pd.clone()) {
                                    let _ = self.pending_decision_tx.send(pd);
                                }
                            }
                        }

                        if let Some(ref cb) = integration.on_failure {
                            let mut params = serde_json::json!({
                                "error": error_str,
                                "integration": integration.name.clone(),
                            });
                            if is_authz_denied {
                                if let Some(ref did) = decision_id {
                                    params["decision_id"] = serde_json::json!(did);
                                }
                                params["authz_denied"] = serde_json::json!(true);
                            }
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
