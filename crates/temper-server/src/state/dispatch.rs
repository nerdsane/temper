//! Action dispatch and WASM integration methods for ServerState.

use std::sync::Arc;
use std::time::Duration;

use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use super::ServerState;
use super::trajectory::TrajectoryEntry;
use crate::entity_actor::{EntityMsg, EntityResponse, EntityState};
use crate::events::EntityStateChange;

impl ServerState {
    /// Dispatch WASM integrations for custom effects produced by a transition.
    ///
    /// For each custom effect matching a WASM integration, this method:
    /// 1. Looks up the integration config from the spec
    /// 2. Looks up the module hash from the WASM registry
    /// 3. Dispatches the callback action (on_success or on_failure) back to the entity
    ///
    /// In production, this would invoke the WASM module via `WasmEngine`.
    /// For now, this records the integration trigger in the trajectory log.
    /// The actual WASM invocation is wired when `temper-wasm` is integrated.
    pub fn dispatch_wasm_integrations(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        _action: &str,
        custom_effects: &[String],
        _entity_state: &EntityState,
    ) {
        // Look up integrations for this entity type
        let integrations = {
            let registry = self.registry.read().unwrap();
            registry
                .get_spec(tenant, entity_type)
                .map(|spec| spec.integrations.clone())
                .unwrap_or_default()
        };

        for effect_name in custom_effects {
            // Find matching WASM integration
            let matching = integrations.iter().find(|ig| {
                ig.integration_type == "wasm" && ig.trigger == *effect_name
            });

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
                let wasm_reg = self.wasm_module_registry.read().unwrap();
                wasm_reg.get_hash(tenant, module_name).is_some()
            };

            if !has_module {
                tracing::warn!(
                    tenant = %tenant,
                    entity_type,
                    module = %module_name,
                    "WASM module not found in registry"
                );

                // Dispatch failure callback asynchronously (fire-and-forget)
                if let Some(ref on_failure) = integration.on_failure {
                    let state = self.clone();
                    let t = tenant.clone();
                    let et = entity_type.to_string();
                    let eid = entity_id.to_string();
                    let cb = on_failure.clone();
                    let int_name = integration.name.clone();
                    let mod_name = module_name.clone();
                    tokio::spawn(async move { // determinism-ok: async callback delivery
                        let fail_params = serde_json::json!({
                            "error": format!("WASM module '{}' not found", mod_name),
                            "integration": int_name,
                        });
                        if let Err(e) = state
                            .dispatch_tenant_action(&t, &et, &eid, &cb, fail_params)
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

            tracing::info!(
                tenant = %tenant,
                entity_type,
                entity_id,
                integration = %integration.name,
                module = %module_name,
                "WASM integration triggered (module invocation pending temper-wasm integration)"
            );

            // Phase 1: dispatch success callback directly (module invocation deferred).
            // When temper-wasm engine is wired in, this block will build a
            // WasmInvocationContext, call engine.invoke(), and choose on_success
            // or on_failure based on the result.
            if let Some(ref on_success) = integration.on_success {
                let state = self.clone();
                let t = tenant.clone();
                let et = entity_type.to_string();
                let eid = entity_id.to_string();
                let cb = on_success.clone();
                let int_name = integration.name.clone();
                let mod_name = module_name.clone();
                tokio::spawn(async move { // determinism-ok: async callback delivery
                    let success_params = serde_json::json!({
                        "integration": int_name,
                        "module": mod_name,
                    });
                    if let Err(e) = state
                        .dispatch_tenant_action(&t, &et, &eid, &cb, success_params)
                        .await
                    {
                        tracing::error!(
                            callback = %cb,
                            error = %e,
                            "failed to dispatch WASM success callback"
                        );
                    }
                });
            }
        }
    }

    /// Dispatch an action to an entity actor (legacy single-tenant).
    pub async fn dispatch_action(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        self.dispatch_tenant_action(&TenantId::default(), entity_type, entity_id, action, params)
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
    ) -> Result<EntityResponse, String> {
        let response = self
            .dispatch_tenant_action_core(tenant, entity_type, entity_id, action, params)
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

        Ok(response)
    }

    /// Core dispatch without reaction cascade (used by ReactionDispatcher to
    /// avoid infinite async recursion).
    pub(crate) async fn dispatch_tenant_action_core(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
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
                Duration::from_secs(5),
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
        };
        if let Err(e) = self.persist_trajectory_entry(&trajectory_entry).await {
            tracing::error!(error = %e, "failed to persist trajectory entry");
        } else if let Ok(mut log) = self.trajectory_log.write() {
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
            tokio::spawn(async move { // determinism-ok: external side-effect, no simulation-visible state
                dispatcher.dispatch(&entry);
            });
        }

        // Dispatch WASM integrations for custom effects (async post-transition).
        // Each integration callback is spawned as a background task internally.
        // This matches the IOA model: output action → environment → input action.
        if response.success && !response.custom_effects.is_empty() {
            self.dispatch_wasm_integrations(
                tenant,
                entity_type,
                entity_id,
                action,
                &response.custom_effects,
                &response.state,
            );
        }

        Ok(response)
    }
}
