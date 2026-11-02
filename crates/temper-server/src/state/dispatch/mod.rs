//! Action dispatch and WASM integration methods for ServerState.

use std::sync::{Arc, Mutex};

use crate::entity_actor::EntityState;
use crate::request_context::AgentContext;
use temper_runtime::tenant::TenantId;
use temper_wasm::{WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate};
use tokio::spawn as spawn_background_wasm_integration; // determinism-ok: external WASM side effects
use tracing::Instrument;

mod actions;
mod adapter;
mod answers;
mod authenticated_params;
pub(crate) use authenticated_params::bind_authenticated_params;
mod compensation;
mod composite;
mod cross_entity;
mod detached;
mod effects;
mod error;
pub use error::DispatchError;
pub(crate) mod entity_rows;
mod generated_callbacks;
mod outside_write;
pub(crate) mod owed_writes;
pub(crate) mod spawn_plan;
pub use outside_write::StaleWrite;
pub(crate) mod retry;
mod scheduled_actions;
pub(crate) mod state_timeouts;
mod state_timer_fire;
mod state_timer_follow;
mod state_timer_reconcile;
mod state_timer_rows;
mod state_timer_setter;
mod state_timer_settled;
mod state_timer_stop;
#[cfg(test)]
#[path = "strict_support_test.rs"]
mod strict_test_support;
mod timer_cause;
mod timer_standing;
mod trigger_failures;
mod wasm;
pub(crate) use wasm::authorized_http_endpoint_host;
#[cfg(feature = "observe")]
pub(crate) use wasm::internal_http_capability_issuer;
mod wasm_secrets;

pub use state_timeouts::StateTimeoutTracker;
pub use state_timer_follow::{BootTimerCounts, BootTimers, TimerFollowHold};

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
    pub dispatch_idempotency_key: Option<&'a str>,
    pub action_params: &'a serde_json::Value,
    pub mode: WasmDispatchMode,
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

/// Options for the extended dispatch entry point.
///
/// Deprecated: prefer [`DispatchCommand`] which unifies all dispatch parameters.
pub struct DispatchExtOptions<'a> {
    pub agent_ctx: &'a AgentContext,
    pub await_integration: bool,
    pub await_reactions: bool,
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
    /// Whether to await cross-entity reaction cascades before returning.
    pub await_reactions: bool,
}

pub(super) fn record_workflow_span_attrs(
    agent_ctx: &AgentContext,
    entity_type: &str,
    entity_id: &str,
    action: Option<&str>,
) {
    let span = tracing::Span::current();
    let root_type = workflow_root_type(agent_ctx, entity_type);
    let root_id = workflow_root_id(agent_ctx, entity_id);
    let run_id = workflow_run_id(agent_ctx, entity_type, entity_id);
    span.record("workflow.root_entity_type", root_type.as_str());
    span.record("workflow.root_entity_id", root_id.as_str());
    span.record("workflow.run_id", run_id.as_str());
    if let Some(action) = action {
        span.record("temper.action", action);
    }
    if let Some(session_id) = agent_ctx.session_id.as_deref() {
        span.record("session.id", session_id);
    }
}

fn workflow_root_type(agent_ctx: &AgentContext, fallback: &str) -> String {
    agent_ctx
        .workflow_root_entity_type
        .clone()
        .unwrap_or_else(|| fallback.to_string())
}

fn workflow_root_id(agent_ctx: &AgentContext, fallback: &str) -> String {
    agent_ctx
        .workflow_root_entity_id
        .clone()
        .unwrap_or_else(|| fallback.to_string())
}

fn workflow_run_id(agent_ctx: &AgentContext, entity_type: &str, entity_id: &str) -> String {
    agent_ctx.workflow_run_id.clone().unwrap_or_else(|| {
        format!(
            "{}:{}",
            workflow_root_type(agent_ctx, entity_type),
            workflow_root_id(agent_ctx, entity_id)
        )
    })
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
        let agent_ctx = agent_ctx.for_background_task();
        let action_params = action_params.clone();
        let workflow_root_entity_type = workflow_root_type(&agent_ctx, &entity_type);
        let workflow_root_entity_id = workflow_root_id(&agent_ctx, &entity_id);
        let workflow_run_id = workflow_run_id(&agent_ctx, &entity_type, &entity_id);
        let span = crate::trace_context::runtime_work_span!(
            crate::trace_context::cause(),
            "dispatch.background_wasm_integrations",
            workflow.root_entity_type = %workflow_root_entity_type,
            workflow.root_entity_id = %workflow_root_entity_id,
            workflow.run_id = %workflow_run_id,
            temper.action = %action,
            entity_type = %entity_type,
            entity_id = %entity_id,
        );
        spawn_background_wasm_integration(
            async move {
                // determinism-ok: async integration side-effects run outside simulation core
                let req = WasmDispatchRequest {
                    tenant: &tenant,
                    entity_type: &entity_type,
                    entity_id: &entity_id,
                    action: &action,
                    custom_effects: &custom_effects,
                    entity_state: &entity_state,
                    agent_ctx: &agent_ctx,
                    dispatch_idempotency_key: None,
                    action_params: &action_params,
                    mode: WasmDispatchMode::Background,
                };
                if let Err(e) = state.dispatch_wasm_integrations_internal(&req).await {
                    tracing::error!(error = %e, "background WASM integration dispatch failed");
                    // ADR-0152: the transition is durable, so dispatch a
                    // compensating failure transition instead of dropping the
                    // error. Never silent: falls back to a surfaced metric +
                    // Observe event when no failure path is declared. This is a
                    // sync method that spawns its own task (like
                    // dispatch_spawn_requests), breaking async recursion.
                    state.dispatch_integration_failure_compensation(
                        &tenant,
                        &entity_type,
                        &entity_id,
                        &action,
                        &e,
                        &agent_ctx,
                    );
                }
            }
            .instrument(span),
        );
    }
}

#[cfg(test)]
#[path = "dispatch_test.rs"]
mod tests;

#[cfg(test)]
#[path = "runtime_work_trace_test.rs"]
mod runtime_work_trace_tests;
