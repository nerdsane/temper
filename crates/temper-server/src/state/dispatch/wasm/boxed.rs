use futures_util::FutureExt;
use futures_util::future::BoxFuture;

use std::sync::Arc;

use super::{
    HttpCallAuthzDenialTracker, WasmDispatchCtx, WasmDispatchMode, WasmDispatchRequest,
    WasmEntityRef,
};
use crate::entity_actor::{EntityResponse, EntityState};
use crate::request_context::AgentContext;
use crate::state::dispatch::DispatchError;
use temper_runtime::tenant::TenantId;
use temper_wasm::{WasmHost, WasmInvocationContext, WasmResourceLimits};

/// Heap-allocate the complete inline WASM integration state machine.
///
/// Keeping this adapter in a child module gives Rust an explicit opaque-type
/// boundary for the callback path that can recursively enter action dispatch.
/// The underlying method retains its tracing instrumentation and semantics.
pub(in crate::state::dispatch) fn dispatch_wasm_integrations_boxed<'a>(
    state: &'a crate::state::ServerState,
    request: &'a WasmDispatchRequest<'a>,
) -> BoxFuture<'a, Result<Option<EntityResponse>, String>> {
    state.dispatch_wasm_integrations_internal(request).boxed()
}

/// Heap-allocate WASM invocation and result handling behind an opaque boundary.
#[expect(
    clippy::too_many_arguments,
    reason = "opaque boundary mirrors the existing instrumented async signature"
)]
pub(in crate::state::dispatch::wasm) fn invoke_and_handle_result_boxed<'a>(
    state: &'a crate::state::ServerState,
    ctx: &'a WasmDispatchCtx<'a>,
    integration: &'a temper_spec::automaton::Integration,
    module_name: &'a str,
    hash: &'a str,
    entity_state: &'a EntityState,
    invocation_context: WasmInvocationContext,
    host: Arc<dyn WasmHost>,
    limits: &'a WasmResourceLimits,
    denial_tracker: &'a HttpCallAuthzDenialTracker,
    blob_cache: std::collections::BTreeMap<String, Vec<u8>>,
    llm_parent_span_id: Option<&'a str>,
) -> BoxFuture<'a, Result<Option<EntityResponse>, String>> {
    state
        .invoke_and_handle_result(
            ctx,
            integration,
            module_name,
            hash,
            entity_state,
            invocation_context,
            host,
            limits,
            denial_tracker,
            blob_cache,
            llm_parent_span_id,
        )
        .boxed()
}

/// Heap-allocate failure recording and its optional recovery callback.
pub(in crate::state::dispatch::wasm) fn handle_wasm_failure_boxed<'a>(
    state: &'a crate::state::ServerState,
    ctx: &'a WasmDispatchCtx<'a>,
    integration_name: &'a str,
    module_name: &'a str,
    on_failure: &'a Option<String>,
    error: String,
    duration_ms: u64,
) -> BoxFuture<'a, Result<Option<EntityResponse>, String>> {
    state
        .handle_wasm_failure(
            ctx,
            integration_name,
            module_name,
            on_failure,
            error,
            duration_ms,
        )
        .boxed()
}

/// Heap-allocate an inline or background WASM callback dispatch.
pub(in crate::state::dispatch) fn dispatch_wasm_callback_boxed<'a>(
    state: &'a crate::state::ServerState,
    entity_ref: WasmEntityRef<'a>,
    callback_action: &'a str,
    callback_params: serde_json::Value,
    agent_context: &'a AgentContext,
    mode: WasmDispatchMode,
) -> BoxFuture<'a, Result<Option<EntityResponse>, String>> {
    state
        .dispatch_wasm_callback(
            entity_ref,
            callback_action,
            callback_params,
            agent_context,
            mode,
        )
        .boxed()
}

/// Heap-allocate recursive core action dispatch from a WASM callback.
#[expect(
    clippy::too_many_arguments,
    reason = "opaque boundary mirrors the existing instrumented async signature"
)]
pub(in crate::state::dispatch) fn dispatch_tenant_action_core_boxed<'a>(
    state: &'a crate::state::ServerState,
    tenant: &'a TenantId,
    entity_type: &'a str,
    entity_id: &'a str,
    action: &'a str,
    params: serde_json::Value,
    agent_context: &'a AgentContext,
    await_integration: bool,
    reaction_context: Option<crate::trigger::delivery::ReactionCommitContext>,
    expected_authorization_precondition: Option<String>,
) -> BoxFuture<'a, Result<EntityResponse, DispatchError>> {
    state
        .dispatch_tenant_action_core(
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_context,
            await_integration,
            reaction_context,
            expected_authorization_precondition,
        )
        .boxed()
}
