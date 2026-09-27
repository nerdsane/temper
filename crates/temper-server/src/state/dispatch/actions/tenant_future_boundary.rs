//! Check the legacy tenant-dispatch future's Send proof once for callers.

use std::future::Future;

use temper_runtime::tenant::TenantId;

use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::ServerState;

impl ServerState {
    /// Convenience wrapper around [`dispatch`](Self::dispatch) for the common
    /// case where `await_integration` is `false`.
    ///
    /// Callers that need integration await or other options should use
    /// `dispatch(DispatchCommand { .. })` directly.
    pub fn dispatch_tenant_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
    ) -> impl Future<Output = Result<EntityResponse, String>> + Send {
        // Returning the original future preserves poll-time tracing, reaction
        // ordering, ownership and cancellation without allocation or eager work.
        self.dispatch_tenant_action_inner(tenant, entity_type, entity_id, action, params, agent_ctx)
    }
}
