//! Allocation-free auto-trait boundaries for the recursive dispatch graph.

use std::future::Future;

use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::{DispatchCommand, DispatchError, ServerState};
use temper_runtime::tenant::TenantId;

impl ServerState {
    /// Dispatch an action using the unified command object.
    ///
    /// This is the preferred entry point. The command struct makes all
    /// parameters explicit (especially tenant) and avoids the previous
    /// three-layer wrapper chain.
    pub fn dispatch(
        &self,
        cmd: DispatchCommand<'_>,
    ) -> impl Future<Output = Result<EntityResponse, String>> + Send {
        // Check Send once here instead of making every handler and caller walk
        // the nested reaction, composite and WASM callback state machines.
        // The async implementation still creates its span and acts only when
        // polled; returning it directly adds no allocation or eager work.
        self.dispatch_inner(cmd)
    }

    /// Core dispatch without reaction cascade (used by ReactionDispatcher to
    /// avoid infinite async recursion).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_tenant_action_core<'dispatch>(
        &'dispatch self,
        tenant: &'dispatch TenantId,
        entity_type: &'dispatch str,
        entity_id: &'dispatch str,
        action: &'dispatch str,
        params: serde_json::Value,
        agent_ctx: &'dispatch AgentContext,
        await_integration: bool,
        expected_authorization_precondition: Option<String>,
    ) -> impl Future<Output = Result<EntityResponse, DispatchError>> + Send + 'dispatch {
        self.dispatch_tenant_action_core_inner(
            DispatchCommand {
                tenant,
                entity_type,
                entity_id,
                action,
                params,
                agent_ctx,
                await_integration,
                await_reactions: false,
            },
            expected_authorization_precondition,
        )
    }
}

#[cfg(test)]
mod tests;
