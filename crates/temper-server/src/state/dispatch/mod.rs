//! Action dispatch and WASM integration methods for ServerState.

use std::sync::{Arc, Mutex};

use crate::dispatch::AgentContext;
use crate::entity_actor::EntityState;
use crate::wasm_authz_gate::PermissiveWasmAuthzGate;
use temper_runtime::tenant::TenantId;
use temper_wasm::{WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate};

mod actions;
mod cross_entity;
mod wasm;
mod wasm_secrets;

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

impl crate::state::ServerState {
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
}
