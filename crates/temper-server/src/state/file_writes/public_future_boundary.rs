//! Expose the public upload future's Send proof without another async layer.

use std::future::Future;

use temper_runtime::tenant::TenantId;

use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::ServerState;

impl ServerState {
    /// Upload stream content for a TemperFS `File` entity, then dispatch the
    /// verified `StreamUpdated` action.
    ///
    /// This is the programmatic equivalent of `PUT /tdata/Files('{id}')/$value`
    /// and keeps platform-side bootstrapping aligned with normal stream writes.
    pub fn put_file_stream_content(
        &self,
        tenant: &TenantId,
        file_id: &str,
        body: &[u8],
        mime_type: &str,
        agent_ctx: &AgentContext,
    ) -> impl Future<Output = Result<EntityResponse, String>> + Send {
        // Keep this opaque contract separate from the checked-upload boundary:
        // validating the public future can use that already-checked Send bound.
        self.put_file_stream_content_inner(tenant, file_id, body, mime_type, agent_ctx)
    }
}
