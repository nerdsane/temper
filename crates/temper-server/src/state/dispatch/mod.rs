//! Action dispatch and WASM integration methods for ServerState.

use std::sync::{Arc, Mutex};

use crate::entity_actor::EntityState;
use crate::request_context::AgentContext;
use temper_runtime::tenant::TenantId;
use temper_wasm::{WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate};

mod actions;
mod adapter;
mod cross_entity;
mod effects;
pub(crate) mod retry;
pub(crate) mod state_timeouts;
mod wasm;
mod wasm_secrets;

pub use state_timeouts::StateTimeoutTracker;

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
///
/// See ADR-0048 (Dispatch retry and error taxonomy) for the classification
/// contract. `Transient` and `Permanent` are the two error shapes an ask
/// site can produce after retry exhaustion; `Deferred` is reserved for
/// ADR-0051 (Admission control).
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The actor was reachable but retry budget was exhausted. The caller
    /// (or intermediary proxy) may retry with a fresh budget.
    ///
    /// `attempts` is how many tries were made before giving up; useful for
    /// metrics and log correlation.
    #[error("actor dispatch exhausted after {attempts} attempt(s): {source}")]
    Transient {
        source: temper_runtime::actor::ActorError,
        attempts: u32,
    },

    /// The actor returned a permanent error — retrying will not help.
    /// Includes stopped / panicked / init-failed / etc.
    #[error("actor dispatch permanently failed: {source}")]
    Permanent {
        source: temper_runtime::actor::ActorError,
    },

    /// Admission control denied the call and the caller should retry
    /// after `retry_after_ms` (ADR-0051).
    #[error("dispatch deferred: retry after {retry_after_ms}ms")]
    #[allow(dead_code)] // ADR-0051 will populate this from the admission layer
    Deferred { retry_after_ms: u64 },

    /// Legacy: generic actor failure as a string. Retained while callers
    /// migrate to `Transient`/`Permanent`. Prefer the classified variants.
    #[deprecated(note = "Use DispatchError::Transient or DispatchError::Permanent (ADR-0048)")]
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

    /// The entity type has no registered spec — all actions are denied by default.
    #[error("entity type '{0}' is not governed by any registered spec")]
    Ungoverned(String),

    /// An internal error (serialization, persistence, unexpected state).
    #[error("{0}")]
    Internal(String),
}

impl DispatchError {
    /// Classify an `ActorError` into the appropriate `DispatchError` variant
    /// based on ADR-0048's transient/permanent taxonomy.
    ///
    /// `attempts` should be the number of retry attempts made before giving
    /// up (1 if no retry was attempted).
    pub(crate) fn from_actor_error(err: temper_runtime::actor::ActorError, attempts: u32) -> Self {
        if err.is_transient() {
            Self::Transient {
                source: err,
                attempts,
            }
        } else {
            Self::Permanent { source: err }
        }
    }
}

#[cfg(test)]
mod dispatch_error_tests {
    use super::*;
    use std::time::Duration;
    use temper_runtime::actor::ActorError;

    #[test]
    fn transient_actor_errors_map_to_transient_dispatch_variant() {
        let transient = ActorError::AskTimeout(Duration::from_secs(5));
        match DispatchError::from_actor_error(transient, 3) {
            DispatchError::Transient { attempts, .. } => assert_eq!(attempts, 3),
            other => panic!("expected Transient, got {other:?}"),
        }
        let mailbox = ActorError::MailboxFull;
        match DispatchError::from_actor_error(mailbox, 1) {
            DispatchError::Transient { attempts, .. } => assert_eq!(attempts, 1),
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn permanent_actor_errors_map_to_permanent_dispatch_variant() {
        for err in [
            ActorError::Stopped,
            ActorError::SendFailed,
            ActorError::Panicked("boom".into()),
            ActorError::InitFailed("init".into()),
            ActorError::MaxRestartsExceeded(4),
            ActorError::custom("misc"),
        ] {
            let label = format!("{err:?}");
            match DispatchError::from_actor_error(err, 7) {
                DispatchError::Permanent { .. } => {}
                other => panic!("expected Permanent for {label}, got {other:?}"),
            }
        }
    }
}

impl crate::state::ServerState {
    /// Build a retry policy for entity-ops ask sites (GetState, UpdateFields,
    /// Delete, etc.). Per-attempt timeout inherits from `action_dispatch_timeout`
    /// so existing server-level tuning still applies.
    ///
    /// See ADR-0048.
    pub(crate) fn dispatch_retry_policy(&self) -> retry::RetryPolicy {
        retry::RetryPolicy {
            per_attempt_timeout: self.action_dispatch_timeout,
            ..retry::RetryPolicy::default()
        }
        .with_attempt_budget_floor()
    }
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
