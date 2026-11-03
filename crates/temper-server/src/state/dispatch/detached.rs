//! A move's work is the server's once it is sent.
//!
//! A caller that hangs up (a site that gives up after its own timeout, a
//! dropped connection) drops its request's future. The move it sent may
//! still commit, and everything after the commit (the state timer, the
//! scheduled actions, reactions, webhooks, integrations, spawns) belongs to
//! that commit, not to the caller. So the dispatch runs on its own task, on
//! owned values, and the request only awaits its answer: dropping the request
//! drops the wait, never the work.

use std::future::Future;
use std::pin::Pin;

use tracing::Instrument;

use super::{DispatchCommand, DispatchError};
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::state::write_record::WriteCaller;
use temper_runtime::tenant::TenantId;

/// A [`DispatchCommand`] that owns its values, so its task outlives the
/// request that sent it.
struct OwnedDispatch {
    tenant: TenantId,
    entity_type: String,
    entity_id: String,
    action: String,
    params: serde_json::Value,
    agent_ctx: AgentContext,
    await_integration: bool,
    await_reactions: bool,
}

impl OwnedDispatch {
    fn new(cmd: DispatchCommand<'_>) -> Self {
        Self {
            tenant: cmd.tenant.clone(),
            entity_type: cmd.entity_type.to_string(),
            entity_id: cmd.entity_id.to_string(),
            action: cmd.action.to_string(),
            params: cmd.params,
            agent_ctx: cmd.agent_ctx.clone(),
            await_integration: cmd.await_integration,
            await_reactions: cmd.await_reactions,
        }
    }

    fn command(&self) -> DispatchCommand<'_> {
        DispatchCommand {
            tenant: &self.tenant,
            entity_type: &self.entity_type,
            entity_id: &self.entity_id,
            action: &self.action,
            params: self.params.clone(),
            agent_ctx: &self.agent_ctx,
            await_integration: self.await_integration,
            await_reactions: self.await_reactions,
        }
    }
}

/// A dispatch's answer, awaited.
type DispatchAnswer<'a> =
    Pin<Box<dyn Future<Output = Result<EntityResponse, DispatchError>> + Send + 'a>>;

impl ServerState {
    /// Dispatch `cmd` on its own task and await its answer. The task runs to
    /// its end whether or not the caller still waits.
    ///
    /// Boxed and named: a dispatch's reactions dispatch again, so an `async
    /// fn` here would have a future whose type contains itself, and the
    /// compiler could not see that its task may move between threads.
    pub(super) fn dispatch_typed_checked<'a>(
        &'a self,
        cmd: DispatchCommand<'a>,
        caller: WriteCaller,
        expected_authorization_precondition: Option<String>,
    ) -> DispatchAnswer<'a> {
        let job = OwnedDispatch::new(cmd);
        let server = self.clone();
        let work = async move {
            server
                .dispatch_typed_attached(job.command(), caller, expected_authorization_precondition)
                .await
        }
        .instrument(tracing::Span::current());
        Box::pin(async move {
            // Under simulation this runs on the test's single-threaded
            // runtime, as the actor tasks do.
            let ended = tokio::spawn(work).await; // determinism-ok: the move's own task, awaited
            match ended {
                Ok(answer) => answer,
                Err(error) => Err(DispatchError::Internal(format!(
                    "the dispatch ended without an answer: {error}"
                ))),
            }
        })
    }
}
