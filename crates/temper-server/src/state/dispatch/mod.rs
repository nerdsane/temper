//! Action dispatch and WASM integration methods for ServerState.

use std::sync::{Arc, Mutex};

use crate::entity_actor::EntityState;
use crate::request_context::AgentContext;
use temper_runtime::tenant::TenantId;
use temper_wasm::{WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate};

mod actions;
mod cross_entity;
mod effects;
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

/// Unified request for WASM integration dispatch.
///
/// Used by both the background (fire-and-forget) and blocking (inline-await)
/// dispatch paths, replacing the previous 9-parameter positional signature.
pub(crate) struct WasmDispatchRequest<'a> {
    pub tenant: &'a TenantId,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub custom_effects: &'a [String],
    pub entity_state: &'a EntityState,
    pub agent_ctx: &'a AgentContext,
    pub action_params: &'a serde_json::Value,
    pub mode: WasmDispatchMode,
}

/// Typed error enum for action dispatch failures.
///
/// Replaces bare `String` errors in the dispatch chain with structured
/// variants that preserve error context and enable pattern matching at
/// the HTTP boundary.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The actor mailbox ask failed (timeout, mailbox full, actor stopped).
    #[error("actor dispatch failed: {0}")]
    ActorFailed(String),

    /// A WASM integration invocation or callback failed.
    #[error("wasm integration failed: {0}")]
    #[allow(dead_code)] // Reserved for structured error handling migration
    WasmFailed(String),

    /// An authorization check denied the action.
    #[error("authorization denied: {0}")]
    #[allow(dead_code)] // Reserved for structured error handling migration
    AuthzDenied(String),

    /// An internal error (serialization, persistence, unexpected state).
    #[error("{0}")]
    Internal(String),
}

impl From<String> for DispatchError {
    fn from(s: String) -> Self {
        Self::Internal(s)
    }
}

/// Options for the extended dispatch entry point.
///
/// Deprecated: prefer [`DispatchCommand`] which unifies all dispatch parameters.
pub struct DispatchExtOptions<'a> {
    pub agent_ctx: &'a AgentContext,
    pub await_integration: bool,
}

/// Unified command object for entity action dispatch.
///
/// Collapses the previous three-layer dispatch API (`dispatch_action` →
/// `dispatch_tenant_action` → `dispatch_tenant_action_ext`) into a single
/// explicit parameter struct. All callers should migrate to
/// [`ServerState::dispatch`].
pub struct DispatchCommand<'a> {
    /// Target tenant (required — no implicit defaults).
    pub tenant: &'a TenantId,
    /// Entity type name.
    pub entity_type: &'a str,
    /// Entity instance ID.
    pub entity_id: &'a str,
    /// Action to dispatch.
    pub action: &'a str,
    /// Action parameters.
    pub params: serde_json::Value,
    /// Agent identity context.
    pub agent_ctx: &'a AgentContext,
    /// Whether to await WASM integration callbacks before returning.
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
    /// Always returns `CedarWasmAuthzGate` so host calls consistently use
    /// Cedar default-deny semantics when no permit policies match.
    pub(crate) fn wasm_authz_gate(&self) -> Arc<dyn WasmAuthzGate> {
        Arc::new(crate::authz::wasm_gate::CedarWasmAuthzGate::new(
            self.authz.clone(),
        ))
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
            if let Err(e) = state.dispatch_wasm_integrations_internal(&req).await {
                tracing::error!(error = %e, "background WASM integration dispatch failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;

    fn test_state() -> crate::state::ServerState {
        let csdl_xml = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
        let csdl = parse_csdl(csdl_xml).expect("CSDL should parse");
        crate::state::ServerState::new(
            ActorSystem::new("dispatch-wasm-authz-test"),
            csdl,
            csdl_xml.to_string(),
        )
    }

    #[test]
    fn wasm_authz_gate_evaluates_cedar_when_policy_set_is_empty() {
        let state = test_state();
        state
            .authz
            .reload_policies("")
            .expect("empty policy set should parse");

        let gate = state.wasm_authz_gate();
        let decision = gate.authorize_http_call(
            "api.example.com",
            "GET",
            "https://api.example.com/v1/ping",
            &WasmAuthzContext::test_fixture(),
        );

        assert_eq!(
            decision,
            WasmAuthzDecision::Deny("no matching permit policy".to_string())
        );
    }

    #[test]
    fn wasm_authz_gate_allows_when_cedar_policy_matches() {
        let state = test_state();
        state
            .authz
            .reload_policies(
                r#"
                permit(
                    principal is Agent,
                    action == Action::"http_call",
                    resource is HttpEndpoint
                ) when {
                    context.module == "stripe_charge"
                };
                "#,
            )
            .expect("policy should parse");

        let gate = state.wasm_authz_gate();
        let decision = gate.authorize_http_call(
            "api.stripe.com",
            "POST",
            "https://api.stripe.com/v1/charges",
            &WasmAuthzContext::test_fixture(),
        );

        assert_eq!(decision, WasmAuthzDecision::Allow);
    }
}
