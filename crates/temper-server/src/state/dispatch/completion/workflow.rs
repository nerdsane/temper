//! Workflow trace housekeeping follows execution ownership, not its observers.
use crate::{entity_actor::EntityResponse, idempotency::OperationRequest, state::ServerState};
use std::sync::Arc;

pub(super) struct CompletionWorkflow {
    registry: Arc<crate::workflow_tracing::WorkflowSpanRegistry>,
    identity: Option<(String, String, String)>,
    status: String,
}

impl CompletionWorkflow {
    pub(super) fn new(
        state: &ServerState,
        request: &OperationRequest,
        response: &EntityResponse,
    ) -> Self {
        let identity = request
            .agent_ctx
            .workflow_run_id
            .clone()
            .map(|run| (run, request.entity_type.clone(), request.entity_id.clone()));
        Self {
            registry: state.workflow_spans.clone(),
            identity,
            status: response.state.status.clone(),
        }
    }

    pub(super) fn finish(&mut self, response: &EntityResponse) {
        self.status = response.state.status.clone();
        self.schedule();
    }

    fn schedule(&mut self) {
        if let Some((run, entity_type, entity_id)) = &self.identity {
            self.registry.finish_if_terminal_after_drain(
                run.clone(),
                entity_type.clone(),
                entity_id.clone(),
                self.status.clone(),
            );
        }
    }
}

impl Drop for CompletionWorkflow {
    fn drop(&mut self) {
        // An interrupted owner still has a last known business response. The
        // registry's existing terminal predicate remains the sole authority.
        self.schedule();
    }
}
