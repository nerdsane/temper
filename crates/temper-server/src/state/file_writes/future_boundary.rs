//! Check the shared upload future's Send proof once for stream and platform callers.

use std::future::Future;

use temper_runtime::tenant::TenantId;

use super::FileStreamContentError;
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::ServerState;

impl ServerState {
    /// Return the unchanged upload future with its Send proof exposed.
    pub(crate) fn put_file_stream_content_checked(
        &self,
        tenant: &TenantId,
        file_id: &str,
        body: &[u8],
        mime_type: &str,
        agent_ctx: &AgentContext,
        expected_authorization_precondition: Option<String>,
    ) -> impl Future<Output = Result<EntityResponse, FileStreamContentError>> + Send {
        // No polling, allocation, or extra async layer: workspace guards, blob
        // writes, authorization preconditions, callbacks and cancellation keep
        // their original order and remain deferred until the future is polled.
        self.put_file_stream_content_checked_inner(
            tenant,
            file_id,
            body,
            mime_type,
            agent_ctx,
            expected_authorization_precondition,
        )
    }
}

#[cfg(test)]
mod tests;
