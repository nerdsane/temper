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

impl ServerState {
    /// Dispatch WASM integrations inline (blocking), returning the final
    /// post-callback `EntityResponse` if any integration matched.
    ///
    /// Unlike `dispatch_wasm_integrations` which runs in background,
    /// this method awaits each WASM invocation and callback dispatch inline.
    /// Returns `Ok(None)` if no integrations matched.
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
            self.dispatch_wasm_integrations_internal(
                tenant,
                entity_type,
                entity_id,
                action,
                custom_effects,
                entity_state,
                agent_ctx,
                action_params,
                WasmDispatchMode::Inline,
            )
            .await
        })
    }
}
