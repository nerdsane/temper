//! Action dispatch and WASM integration methods for ServerState.

use std::sync::{Arc, Mutex};

use crate::entity_actor::EntityState;
use crate::request_context::AgentContext;
use temper_runtime::tenant::TenantId;
use temper_wasm::{WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate};
use tracing::Instrument;

mod actions;
mod adapter;
mod compensation;
mod composite;
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
    pub dispatch_idempotency_key: Option<&'a str>,
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

    /// An authorization check refused the action.
    ///
    /// `class` preserves *why* it was refused: a Cedar policy decision, a
    /// request the engine could not evaluate, or an engine failure. Without
    /// it the HTTP boundary reports an engine crash as "denied by policy",
    /// which sends the caller chasing a policy that does not exist (ARN-287).
    #[error("authorization denied: {reason}")]
    AuthzDenied {
        class: temper_authz::DenialClass,
        reason: String,
    },

    /// A tenant or owner quota denied the action before dispatch.
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    /// The request conflicts with an existing governed row.
    #[error("conflict: {0}")]
    Conflict(String),

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
        use temper_runtime::actor::InitFailureKind;
        for err in [
            ActorError::Stopped,
            ActorError::SendFailed,
            ActorError::Panicked("boom".into()),
            ActorError::init_failed("init", InitFailureKind::Defect),
            ActorError::init_failed("dup key", InitFailureKind::Constraint),
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

    #[test]
    fn an_init_failure_on_a_dependency_maps_to_transient() {
        // A store blip during spawn is retryable; before the taxonomy split
        // every init failure was permanent and this dispatch was given up on.
        let err = ActorError::init_failed(
            "store unavailable",
            temper_runtime::actor::InitFailureKind::TransientDependency,
        );
        match DispatchError::from_actor_error(err, 2) {
            DispatchError::Transient { attempts, .. } => assert_eq!(attempts, 2),
            other => panic!("expected Transient, got {other:?}"),
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
        let agent_ctx = agent_ctx.clone();
        let action_params = action_params.clone();
        let workflow_root_entity_type = workflow_root_type(&agent_ctx, &entity_type);
        let workflow_root_entity_id = workflow_root_id(&agent_ctx, &entity_id);
        let workflow_run_id = workflow_run_id(&agent_ctx, &entity_type, &entity_id);
        let span = tracing::info_span!(
            "dispatch.background_wasm_integrations",
            workflow.root_entity_type = %workflow_root_entity_type,
            workflow.root_entity_id = %workflow_root_entity_id,
            workflow.run_id = %workflow_run_id,
            temper.action = %action,
            entity_type = %entity_type,
            entity_id = %entity_id,
        );
        tokio::spawn(
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
                    );
                }
            }
            .instrument(span),
        );
    }
}

#[cfg(test)]
#[path = "dispatch_mod_test.rs"]
mod tests;
