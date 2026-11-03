//! A reaction's move on its target, owed once the source move commits.
//!
//! Each try reads the target, authorizes the move on it as it is now, and
//! sends the move through the core dispatch, without its own reaction
//! cascade: [`super::ReactionDispatcher`] runs the cascade of a move that
//! lands at once. A try the store failed, in the read or the send, is kept
//! (`state::dispatch::owed_writes`) and tried again under the reaction's key
//! until it lands or fails otherwise, and its cascade runs when it lands.

use std::sync::{Arc, PoisonError};

use temper_authz::SecurityContext;

use crate::entity_actor::EntityResponse;
use crate::state::owed_writes::{Attempt, OwedKind, OwedMove, OwedTry};
use crate::state::{DispatchError, ServerState};

/// What one try of a reaction did.
pub(crate) enum ReactionTry {
    /// Its target could not be read to authorize the move on it.
    Unread(DispatchError),
    /// Cedar refused the move on its target, in `target_status`.
    Denied {
        reason: String,
        target_status: String,
    },
    /// The move was sent: the dispatch's reply.
    Sent(Box<Result<EntityResponse, DispatchError>>),
}

impl ReactionTry {
    fn attempt(&self) -> Attempt {
        match self {
            Self::Unread(error) => Attempt::of_error(error),
            Self::Denied { reason, .. } => Attempt::Failed(reason.clone()),
            Self::Sent(reply) => Attempt::of(reply),
        }
    }
}

/// A reaction's move on its target, sent as its context, which carries the
/// reaction's key, and who the move is authorized as.
pub(crate) struct Reaction {
    pub(crate) sent: OwedMove,
    pub(crate) authorized_as: SecurityContext,
}

impl ServerState {
    /// Try `reaction` at cascade `depth`, and keep it if the store failed it.
    pub(crate) async fn dispatch_reaction(&self, reaction: Reaction, depth: u32) -> ReactionTry {
        let tried = try_reaction(self, &reaction).await;
        if let Attempt::StoreFailed(error) = tried.attempt() {
            let owed = reaction.sent.owed(OwedKind::Reaction);
            let reaction = Arc::new(reaction);
            self.keep_owed_write(owed, &error, move |state| {
                react_again(state, Arc::clone(&reaction), depth)
            });
        }
        tried
    }
}

/// Read the reaction's target, authorize its move on the target as it is
/// now, and send it.
async fn try_reaction(state: &ServerState, reaction: &Reaction) -> ReactionTry {
    let sent = &reaction.sent;
    let snapshot = match state
        .load_authz_resource_snapshot(&sent.tenant, &sent.entity_type, &sent.entity_id)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => return ReactionTry::Unread(error),
    };
    if let Err(denial) = state.authorize_with_context(
        &reaction.authorized_as,
        &sent.action,
        &sent.entity_type,
        &snapshot.resource_attrs,
        sent.tenant.as_str(),
    ) {
        let reason = denial.to_string();
        // Every final Cedar refusal is reported here, including a retry whose
        // first try could not read or send. The dispatcher only counts its
        // immediate results on the fan-out span.
        reaction.record_withheld(
            state,
            &snapshot.current_state.state.status,
            &snapshot.resource_attrs,
            &reason,
        );
        return ReactionTry::Denied {
            reason,
            target_status: snapshot.current_state.state.status,
        };
    }
    let reply = state
        .dispatch_tenant_action_core(
            &sent.tenant,
            &sent.entity_type,
            &sent.entity_id,
            &sent.action,
            sent.params.clone(),
            &sent.agent_ctx,
            false,
            crate::state::write_record::WriteCaller::Kernel,
            None,
        )
        .await;
    if let Ok(response) = &reply
        && response.refused
    {
        reaction.record_refused(state, response);
    }
    ReactionTry::Sent(Box::new(reply))
}

/// Try an owed reaction again; once it lands, run its cascade at the next
/// depth, as the dispatcher does for a reaction that lands at once: under
/// the reaction's context, which knows the children it spawned as made.
fn react_again(state: ServerState, reaction: Arc<Reaction>, depth: u32) -> OwedTry {
    Box::pin(async move {
        let tried = try_reaction(&state, &reaction).await;
        let attempt = tried.attempt();
        let dispatcher = state
            .reaction_dispatcher
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let sent = &reaction.sent;
        if let (ReactionTry::Sent(reply), Some(dispatcher)) = (tried, dispatcher)
            && let Ok(response) = *reply
            && response.success
        {
            let fields = serde_json::to_value(&response.state.fields).unwrap_or_default();
            let cascade_ctx = sent.agent_ctx.clone().having_made(
                response
                    .spawn_requests
                    .iter()
                    .map(|spawn| (spawn.entity_type.as_str(), spawn.entity_id.as_str())),
            );
            Box::pin(dispatcher.dispatch_reactions(
                &state,
                &sent.tenant,
                &sent.entity_type,
                &sent.entity_id,
                response.action(&sent.action),
                &response.state.status,
                &fields,
                depth + 1,
                &cascade_ctx,
            ))
            .await;
        }
        attempt
    })
}
