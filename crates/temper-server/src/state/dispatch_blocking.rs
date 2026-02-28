//! Blocking (inline-await) WASM integration dispatch.
//!
//! When `?await_integration=true` is set, the HTTP handler awaits WASM
//! invocations inline instead of spawning fire-and-forget tasks. This
//! lets agents do "action + integration" in a single request.

use super::ServerState;
use super::dispatch::WasmDispatchMode;
use crate::dispatch::AgentContext;
use crate::entity_actor::{EntityResponse, EntityState};
use temper_runtime::tenant::TenantId;

pub(crate) struct BlockingWasmDispatch<'a> {
    pub tenant: &'a TenantId,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub custom_effects: &'a [String],
    pub entity_state: &'a EntityState,
    pub agent_ctx: &'a AgentContext,
    pub action_params: &'a serde_json::Value,
}

impl ServerState {
    /// Dispatch WASM integrations inline (blocking), returning the final
    /// post-callback `EntityResponse` if any integration matched.
    ///
    /// Unlike `dispatch_wasm_integrations` which runs in background,
    /// this method awaits each WASM invocation and callback dispatch inline.
    /// Returns `Ok(None)` if no integrations matched.
    pub(crate) fn dispatch_wasm_integrations_blocking<'a>(
        &'a self,
        request: BlockingWasmDispatch<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<EntityResponse>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.dispatch_wasm_integrations_internal(
                request.tenant,
                request.entity_type,
                request.entity_id,
                request.action,
                request.custom_effects,
                request.entity_state,
                request.agent_ctx,
                request.action_params,
                WasmDispatchMode::Inline,
            )
            .await
        })
    }
}
