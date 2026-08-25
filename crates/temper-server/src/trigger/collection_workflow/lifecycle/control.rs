//! Cancellation and timeout control mutations.

use temper_runtime::persistence::schema_deployment::SchemaEventPin;

use super::super::identity::collection_control_id;
use super::super::model::*;
use crate::trigger::delivery::ReactionDeliveryStatus;

impl CollectionWorkflowRecordV1 {
    /// Apply the first cancellation or timeout request and fence later admission.
    pub(crate) fn request_control(
        &mut self,
        requested_outcome: CollectionRequestedOutcome,
        timeout_delivery_id: Option<&str>,
        source_action: String,
        source_sequence: u64,
        authority: serde_json::Value,
        schema_pin: Option<SchemaEventPin>,
    ) -> Result<(CollectionControlIntentV1, CollectionMutationOutcome), String> {
        match requested_outcome {
            CollectionRequestedOutcome::Cancelled if timeout_delivery_id.is_some() => {
                return Err("cancellation cannot claim timeout delivery evidence".to_string());
            }
            CollectionRequestedOutcome::TimedOut => {
                let binding = self.timeout_binding.as_ref().ok_or_else(|| {
                    "collection timeout is not bound to an ADR-0178 intent".to_string()
                })?;
                if timeout_delivery_id != Some(binding.delivery_id.as_str())
                    || source_action != binding.timeout_action
                {
                    return Err(
                        "StaleCollectionClock: timeout evidence does not own this workflow clock"
                            .to_string(),
                    );
                }
            }
            CollectionRequestedOutcome::Cancelled => {}
        }
        if source_action.is_empty() || source_sequence == 0 {
            return Err(
                "control source action and sequence must be committed evidence".to_string(),
            );
        }
        let control_id = collection_control_id(
            &self.workflow_id,
            &source_action,
            source_sequence,
            requested_outcome.identity_component(),
        );
        if let Some(first) = self.requested_outcome {
            let outcome = if first == requested_outcome
                && self.last_control_id.as_deref() == Some(control_id.as_str())
            {
                CollectionMutationOutcome::Replayed
            } else {
                CollectionMutationOutcome::IgnoredAfterFirstControl
            };
            return Ok((
                self.control_intent(
                    control_id,
                    requested_outcome,
                    source_action,
                    source_sequence,
                    authority,
                    schema_pin,
                ),
                outcome,
            ));
        }
        if self.status != CollectionWorkflowStatus::Running {
            return Err("terminal workflow cannot accept control".to_string());
        }
        self.control_epoch = self
            .control_epoch
            .checked_add(1)
            .ok_or_else(|| "collection control epoch exhausted".to_string())?;
        self.requested_outcome = Some(requested_outcome);
        self.last_control_id = Some(control_id.clone());
        self.control_source_action = Some(source_action.clone());
        self.control_source_sequence = Some(source_sequence);
        self.control_authority = Some(authority.clone());
        self.control_schema_pin = schema_pin.clone();
        self.control_timeout_delivery_id = timeout_delivery_id.map(str::to_string);
        self.status = match requested_outcome {
            CollectionRequestedOutcome::Cancelled => CollectionWorkflowStatus::Cancelling,
            CollectionRequestedOutcome::TimedOut => CollectionWorkflowStatus::TimingOut,
        };
        for member in &mut self.members {
            if member.status == CollectionMemberStatus::Pending
                || (member.status == CollectionMemberStatus::InFlight && member.receipt.is_none())
            {
                member.status = match requested_outcome {
                    CollectionRequestedOutcome::Cancelled => CollectionMemberStatus::Cancelled,
                    CollectionRequestedOutcome::TimedOut => CollectionMemberStatus::TimedOut,
                };
                if member.delivery_id.is_some() {
                    member.delivery_status = Some(ReactionDeliveryStatus::Skipped);
                    member.terminal_control_epoch = Some(self.control_epoch);
                }
            }
        }
        self.next_undispatched_index = self.members.len() as u16;
        self.recount();
        self.classify_if_complete();
        self.validate()?;
        Ok((
            self.control_intent(
                control_id,
                requested_outcome,
                source_action,
                source_sequence,
                authority,
                schema_pin,
            ),
            CollectionMutationOutcome::Applied,
        ))
    }

    fn control_intent(
        &self,
        control_id: String,
        requested_outcome: CollectionRequestedOutcome,
        source_action: String,
        source_sequence: u64,
        authority: serde_json::Value,
        schema_pin: Option<SchemaEventPin>,
    ) -> CollectionControlIntentV1 {
        CollectionControlIntentV1 {
            version: COLLECTION_LEDGER_VERSION,
            control_id,
            workflow_id: self.workflow_id.clone(),
            requested_outcome,
            timeout_delivery_id: self.control_timeout_delivery_id.clone(),
            source_action,
            source_sequence,
            control_epoch: self.control_epoch,
            authority,
            schema_pin,
        }
    }
}
