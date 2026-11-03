//! A write from outside the kernel is checked on the record it will store,
//! and applies only to the record it read.
//!
//! The check builds the record from the state it reads and the dispatch then
//! holds the actor to that state (or to the state Cedar authorized, when the
//! caller brings one). When the record moved on first, the actor applies
//! nothing and answers with the record as it now is, and the dispatch answers
//! the write as stale with that record's version. The route that took the
//! write decides it again on that record, checks and all (`state::redecide`),
//! so a write is never applied to a record its checks did not read, and one
//! its rules allow lands. Only a write still stale at the route's deadline
//! reaches its caller as `StaleWrite`.

use serde_json::{Value, json};

use temper_jit::table::{RelatedMap, TransitionTable};

use super::DispatchError;
use super::effects::PostDispatchContext;
use crate::entity_actor::EntityResponse;
use crate::entity_actor::effects::entity_authorization_precondition;
use crate::state::ServerState;
use crate::state::write_record::{ActionPreview, ActionRecordError, WriteCaller};

impl ServerState {
    /// Check the record an outside action will store, before it is sent to
    /// the actor. A violation is the refusal returned; otherwise `held_to`
    /// becomes the state the check read, unless the caller already holds the
    /// actor to the state it authorized. Kernel follow-ons and types with
    /// nothing to check pass through untouched.
    pub(super) async fn check_outside_action(
        &self,
        ctx: &PostDispatchContext<'_>,
        caller: WriteCaller,
        table: &TransitionTable,
        related: &RelatedMap,
        held_to: &mut Option<String>,
    ) -> Result<Option<EntityResponse>, DispatchError> {
        let PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action,
            agent_ctx,
            action_params,
            ..
        } = *ctx;
        if !self.action_record_checked(caller, tenant, entity_type, action) {
            return Ok(None);
        }
        let current = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
            .map_err(DispatchError::Internal)?;
        // A retry of an action already applied returns its first result.
        if agent_ctx
            .idempotency_key
            .as_ref()
            .map(crate::idempotency::IdempotencyKey::as_str)
            .is_some_and(|key| current.state.has_processed_idempotency_key(key))
        {
            return Ok(None);
        }
        let preview = ActionPreview {
            table,
            current: &current.state,
            action,
            params: action_params,
            related,
            agent_ctx,
        };
        if let Err(error) = self.check_action_record(caller, tenant, preview).await {
            let mut current = current;
            let message = match error {
                ActionRecordError::Constraint(violation) => violation.message,
                ActionRecordError::Preview(result) => {
                    current.refusal = result.refusal;
                    result.error.expect("a refused preview carries its reason")
                }
            };
            return Ok(Some(self.refuse_before_actor(ctx, current, message).await));
        }
        held_to.get_or_insert_with(|| entity_authorization_precondition(&current.state));
        Ok(None)
    }
}

/// A refused write's record had moved past the state the write read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{entity_type} '{entity_id}' changed after this write read it; re-read it and retry (current version {current_version})"
)]
pub struct StaleWrite {
    /// The record's entity type.
    pub entity_type: String,
    /// The record's id.
    pub entity_id: String,
    /// The record's version (its event sequence number) when it was refused.
    pub current_version: u64,
}

impl StaleWrite {
    /// The response body: code `StaleWrite` and the record's current version,
    /// in the shape of the other structured refusals (`ConstraintViolation`).
    pub fn body(&self) -> Value {
        json!({
            "error": {
                "code": "StaleWrite",
                "message": self.to_string(),
                "details": {
                    "entity_type": self.entity_type,
                    "entity_id": self.entity_id,
                    "current_version": self.current_version,
                }
            }
        })
    }
}

/// The stale refusal for an outside write held to the state `read`, when
/// the actor refused it on another state. A refusal on the state the write
/// read is the action's own answer and is kept.
pub(super) fn stale_refusal(
    caller: WriteCaller,
    read: Option<&str>,
    entity_type: &str,
    entity_id: &str,
    response: &EntityResponse,
) -> Option<DispatchError> {
    read?;
    // Only the actor can say the held write did not land. A changed state
    // beside a store failure may instead hold a write whose reply was lost.
    if caller != WriteCaller::Outside || !response.is_record_changed() {
        return None;
    }
    Some(DispatchError::StaleWrite(StaleWrite {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
        current_version: response.state.sequence_nr,
    }))
}

#[cfg(test)]
#[path = "outside_write_test.rs"]
mod tests;
