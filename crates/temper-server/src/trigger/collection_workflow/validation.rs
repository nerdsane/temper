//! Replay validation and aggregate recounting for workflow records.

use super::identity::{collection_control_id, collection_member_id, collection_workflow_id};
use super::model::*;
use crate::trigger::delivery::ReactionDeliveryStatus;

impl CollectionWorkflowRecordV1 {
    /// Recheck all persisted invariants after decode or mutation.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != COLLECTION_LEDGER_VERSION {
            return Err(format!(
                "unsupported collection ledger version {}",
                self.version
            ));
        }
        self.budgets.validate()?;
        if let Some(actions) = &self.execution_actions
            && [
                &actions.member_entity,
                &actions.member_action,
                &actions.member_cancel_action,
                &actions.timeout_action,
                &actions.on_success,
                &actions.on_partial_failure,
                &actions.on_failure,
                &actions.on_cancelled,
                &actions.on_timed_out,
            ]
            .into_iter()
            .any(|action| action.is_empty())
        {
            return Err("collection execution action names must be non-empty".to_string());
        }
        if let Some(timeout) = &self.timeout_binding
            && (timeout.delivery_id.is_empty()
                || timeout.timeout_action.is_empty()
                || timeout.state.is_empty()
                || timeout.declaration_id.is_empty()
                || timeout.clock_sequence != self.source_sequence
                || timeout.schema_digest != self.schema_digest)
        {
            return Err("persisted collection timeout binding is invalid".to_string());
        }
        if self.sealed_roster.is_empty()
            || self.sealed_roster.len() != self.members.len()
            || self.members.len() > usize::from(self.budgets.max_members)
        {
            return Err("sealed roster and member ledger disagree".to_string());
        }
        if usize::from(self.next_undispatched_index) > self.members.len() {
            return Err("roster cursor is outside the sealed roster".to_string());
        }
        let expected_workflow_id = collection_workflow_id(
            &self.tenant,
            &self.source_entity_type,
            &self.source_entity_id,
            &self.declaration_name,
            &self.source_action,
            self.source_sequence,
            &self.schema_digest,
        );
        if expected_workflow_id != self.workflow_id {
            return Err("workflow identity does not match immutable inputs".to_string());
        }
        let mut unique = std::collections::BTreeSet::new();
        for (index, member) in self.members.iter().enumerate() {
            let value = &self.sealed_roster[index];
            let expected_member_id = collection_member_id(&self.workflow_id, index as u32, value);
            if value != &member.member_value
                || member.member_index != index as u32
                || member.member_id != expected_member_id
                || member.child_entity_id != expected_member_id
                || !unique.insert(value)
            {
                return Err("member ledger does not match the sealed roster".to_string());
            }
            if member.attempts > self.budgets.max_attempts {
                return Err("persisted member attempts exceed the workflow budget".to_string());
            }
            validate_member_shape(member, self.requested_outcome, self.control_epoch)?;
        }
        let expected_counts = counts_for(&self.members);
        if self.counts != expected_counts || self.counts.total() != self.members.len() as u16 {
            return Err("persisted workflow counts do not match members".to_string());
        }
        if self.counts.in_flight > u16::from(self.budgets.max_concurrency) {
            return Err("persisted workflow exceeds its concurrency window".to_string());
        }
        let attempts = self
            .members
            .iter()
            .map(|member| u32::from(member.attempts))
            .sum::<u32>();
        if attempts != self.total_attempts {
            return Err("persisted total attempts do not match members".to_string());
        }
        let all_terminal = self.counts.terminal() == self.members.len() as u16;
        let expected_status = match (self.requested_outcome, all_terminal) {
            (Some(CollectionRequestedOutcome::Cancelled), false) => {
                CollectionWorkflowStatus::Cancelling
            }
            (Some(CollectionRequestedOutcome::Cancelled), true) => {
                CollectionWorkflowStatus::Cancelled
            }
            (Some(CollectionRequestedOutcome::TimedOut), false) => {
                CollectionWorkflowStatus::TimingOut
            }
            (Some(CollectionRequestedOutcome::TimedOut), true) => {
                CollectionWorkflowStatus::TimedOut
            }
            (None, false) => CollectionWorkflowStatus::Running,
            (None, true) if self.counts.succeeded == self.members.len() as u16 => {
                CollectionWorkflowStatus::Succeeded
            }
            (None, true) if self.counts.succeeded > 0 => CollectionWorkflowStatus::PartiallyFailed,
            (None, true) => CollectionWorkflowStatus::Failed,
        };
        let expected_classification = if expected_status.is_terminal() {
            Some(expected_status)
        } else {
            None
        };
        if self.status != expected_status || self.terminal_classification != expected_classification
        {
            return Err(
                "workflow lifecycle does not match its durable member partition".to_string(),
            );
        }
        match (self.join_status, self.join_delivery_id.as_deref()) {
            (CollectionJoinStatus::Pending, None) => {}
            (
                CollectionJoinStatus::InFlight
                | CollectionJoinStatus::Delivered
                | CollectionJoinStatus::DeliveryFailed
                | CollectionJoinStatus::SupersededByNewWorkflow,
                Some(id),
            ) if self.status.is_terminal() && !id.is_empty() => {}
            _ => return Err("join lifecycle does not match its delivery identity".to_string()),
        }
        match self.requested_outcome {
            None => {
                if self.control_epoch != 0
                    || self.last_control_id.is_some()
                    || self.control_source_action.is_some()
                    || self.control_source_sequence.is_some()
                    || self.control_authority.is_some()
                    || self.control_schema_pin.is_some()
                    || self.control_timeout_delivery_id.is_some()
                {
                    return Err("uncontrolled workflow contains control evidence".to_string());
                }
            }
            Some(outcome) => {
                let (Some(control_id), Some(action), Some(sequence), Some(_authority)) = (
                    self.last_control_id.as_deref(),
                    self.control_source_action.as_deref(),
                    self.control_source_sequence,
                    self.control_authority.as_ref(),
                ) else {
                    return Err("workflow control evidence is incomplete".to_string());
                };
                if self.control_epoch != 1
                    || sequence <= self.source_sequence
                    || control_id
                        != collection_control_id(
                            &self.workflow_id,
                            action,
                            sequence,
                            outcome.identity_component(),
                        )
                {
                    return Err("workflow control evidence does not match its fence".to_string());
                }
                match outcome {
                    CollectionRequestedOutcome::Cancelled
                        if self.control_timeout_delivery_id.is_some() =>
                    {
                        return Err("cancellation contains timeout ownership evidence".to_string());
                    }
                    CollectionRequestedOutcome::TimedOut
                        if self
                            .timeout_binding
                            .as_ref()
                            .map(|binding| binding.delivery_id.as_str())
                            != self.control_timeout_delivery_id.as_deref() =>
                    {
                        return Err(
                            "timeout control does not match its ADR-0178 binding".to_string()
                        );
                    }
                    _ => {}
                }
            }
        }
        validate_cursor(self)?;
        Ok(())
    }

    pub(super) fn recount(&mut self) {
        self.counts = counts_for(&self.members);
        self.total_attempts = self
            .members
            .iter()
            .map(|member| u32::from(member.attempts))
            .sum();
    }
}

fn validate_cursor(record: &CollectionWorkflowRecordV1) -> Result<(), String> {
    for (index, member) in record.members.iter().enumerate() {
        if record.requested_outcome.is_none()
            && ((index < usize::from(record.next_undispatched_index)
                && member.status == CollectionMemberStatus::Pending)
                || (index >= usize::from(record.next_undispatched_index)
                    && member.status != CollectionMemberStatus::Pending))
        {
            return Err("workflow cursor does not partition admitted roster members".to_string());
        }
    }
    if record.requested_outcome.is_some()
        && usize::from(record.next_undispatched_index) != record.members.len()
    {
        return Err("controlled workflow cursor must fence the complete roster".to_string());
    }
    Ok(())
}

fn validate_member_shape(
    member: &CollectionMemberRecord,
    requested_outcome: Option<CollectionRequestedOutcome>,
    control_epoch: u64,
) -> Result<(), String> {
    let receipt_matches = member.receipt.as_ref().is_none_or(|receipt| {
        member.delivery_id.as_deref() == Some(receipt.delivery_id.as_str())
            && receipt.fencing_token > 0
    });
    if !receipt_matches
        || member
            .admission_control_epoch
            .is_some_and(|epoch| epoch > control_epoch)
        || member
            .terminal_control_epoch
            .is_some_and(|epoch| epoch > control_epoch)
    {
        return Err("persisted member evidence does not match its workflow fence".to_string());
    }
    match member.status {
        CollectionMemberStatus::Pending => {
            if member.attempts != 0
                || member.admission_control_epoch.is_some()
                || member.terminal_control_epoch.is_some()
                || member.delivery_id.is_some()
                || member.cancellation_delivery_id.is_some()
                || member.delivery_status.is_some()
                || member.receipt.is_some()
                || member.failure_class.is_some()
                || (requested_outcome.is_none() && member.cancellation_delivery_id.is_some())
                || requested_outcome.is_some()
            {
                return Err("pending member contains lifecycle evidence".to_string());
            }
        }
        CollectionMemberStatus::InFlight => {
            if member.admission_control_epoch.is_none()
                || member.terminal_control_epoch.is_some()
                || member.delivery_id.is_none()
                || !matches!(
                    member.delivery_status,
                    Some(
                        ReactionDeliveryStatus::Pending
                            | ReactionDeliveryStatus::Claimed
                            | ReactionDeliveryStatus::Dispatching
                    )
                )
                || member.failure_class.is_some()
                || (member.cancellation_delivery_id.is_some()
                    && (requested_outcome.is_none() || member.receipt.is_none()))
                || (requested_outcome.is_some() && member.receipt.is_none())
            {
                return Err("in-flight member evidence is inconsistent".to_string());
            }
        }
        CollectionMemberStatus::Succeeded => {
            if member.attempts == 0
                || member.admission_control_epoch.is_none()
                || member.terminal_control_epoch.is_none()
                || member.delivery_id.is_none()
                || member.delivery_status != Some(ReactionDeliveryStatus::Succeeded)
                || member.receipt.is_none()
                || member.failure_class.is_some()
            {
                return Err("succeeded member lacks exact receipt evidence".to_string());
            }
        }
        CollectionMemberStatus::Failed => {
            if member.attempts == 0
                || member.admission_control_epoch.is_none()
                || member.terminal_control_epoch.is_none()
                || member.delivery_id.is_none()
                || !member.delivery_status.is_some_and(failed_delivery_status)
                || member.failure_class.is_none()
                || (member.cancellation_delivery_id.is_some()
                    && member.failure_class != Some(CollectionFailureClass::CancellationFailed))
            {
                return Err("failed member lacks terminal failure evidence".to_string());
            }
        }
        CollectionMemberStatus::Cancelled | CollectionMemberStatus::TimedOut => {
            let expected = if member.status == CollectionMemberStatus::Cancelled {
                CollectionRequestedOutcome::Cancelled
            } else {
                CollectionRequestedOutcome::TimedOut
            };
            if requested_outcome != Some(expected) || member.failure_class.is_some() {
                return Err("controlled member does not match requested outcome".to_string());
            }
            let undispatched = member.attempts == 0
                && member.admission_control_epoch.is_none()
                && member.terminal_control_epoch.is_none()
                && member.delivery_id.is_none()
                && member.delivery_status.is_none()
                && member.receipt.is_none()
                && member.cancellation_delivery_id.is_none();
            let fenced_before_receipt = member.admission_control_epoch.is_some()
                && member.terminal_control_epoch == Some(control_epoch)
                && member.delivery_id.is_some()
                && member.delivery_status == Some(ReactionDeliveryStatus::Skipped)
                && member.receipt.is_none()
                && member.cancellation_delivery_id.is_none();
            let cancelled_after_receipt = member.admission_control_epoch.is_some()
                && member.terminal_control_epoch == Some(control_epoch)
                && member.delivery_id.is_some()
                && member.delivery_status.is_some()
                && member.receipt.is_some()
                && member.cancellation_delivery_id.is_some();
            if !undispatched && !fenced_before_receipt && !cancelled_after_receipt {
                return Err("controlled member has ambiguous terminal evidence".to_string());
            }
        }
    }
    Ok(())
}

fn failed_delivery_status(status: ReactionDeliveryStatus) -> bool {
    matches!(
        status,
        ReactionDeliveryStatus::Skipped
            | ReactionDeliveryStatus::DroppedAllowed
            | ReactionDeliveryStatus::Rejected
            | ReactionDeliveryStatus::DeadLettered
    )
}

fn counts_for(members: &[CollectionMemberRecord]) -> CollectionWorkflowCounts {
    let mut counts = CollectionWorkflowCounts::default();
    for member in members {
        match member.status {
            CollectionMemberStatus::Pending => counts.pending += 1,
            CollectionMemberStatus::InFlight => counts.in_flight += 1,
            CollectionMemberStatus::Succeeded => counts.succeeded += 1,
            CollectionMemberStatus::Failed => counts.failed += 1,
            CollectionMemberStatus::Cancelled => counts.cancelled += 1,
            CollectionMemberStatus::TimedOut => counts.timed_out += 1,
        }
    }
    counts
}
