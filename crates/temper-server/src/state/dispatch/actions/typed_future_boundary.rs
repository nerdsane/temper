//! Check typed dispatch's Send proof once for conditional mutation callers.

use std::future::Future;

use crate::entity_actor::EntityResponse;
use crate::state::{DispatchCommand, DispatchError, ServerState};

impl ServerState {
    /// Return the unchanged checked-dispatch future with its Send proof exposed.
    pub(super) fn dispatch_typed_checked(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: Option<String>,
    ) -> impl Future<Output = Result<EntityResponse, DispatchError>> + Send {
        // Returning the existing async implementation neither polls it nor
        // allocates another future. Preconditions, reactions and cancellation
        // retain their existing execution order.
        self.dispatch_typed_checked_inner(cmd, expected_authorization_precondition)
    }
}
