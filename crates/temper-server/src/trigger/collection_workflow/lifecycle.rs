//! Lifecycle mutations and invariant validation for collection records.

mod control;
mod join;

use super::identity::{collection_member_id, collection_workflow_id};
use super::model::*;
use crate::trigger::delivery::ReactionDeliveryStatus;

impl CollectionWorkflowRecordV1 {
    /// Validate and seal a roster directly into `Running` state.
    pub(crate) fn start(
        start: CollectionWorkflowStart,
    ) -> Result<(CollectionStartIntentV1, Self), String> {
        let budgets = start.budgets.validate()?;
        for value in [
            start.tenant.as_str(),
            start.source_entity_type.as_str(),
            start.source_entity_id.as_str(),
            start.declaration_name.as_str(),
            start.source_action.as_str(),
            start.schema_digest.as_str(),
        ] {
            if value.is_empty() {
                return Err("collection identity components must be non-empty".to_string());
            }
        }
        if start.source_sequence == 0 {
            return Err("source_sequence must identify a committed event".to_string());
        }
        if start.roster.is_empty() || start.roster.len() > usize::from(budgets.max_members) {
            return Err("roster must contain 1..=max_members values".to_string());
        }
        let mut unique = std::collections::BTreeSet::new();
        for value in &start.roster {
            if value.is_empty() {
                return Err("roster values must be non-empty strings".to_string());
            }
            if !unique.insert(value.as_str()) {
                return Err("roster values must be unique".to_string());
            }
        }

        let workflow_id = collection_workflow_id(
            &start.tenant,
            &start.source_entity_type,
            &start.source_entity_id,
            &start.declaration_name,
            &start.source_action,
            start.source_sequence,
            &start.schema_digest,
        );
        let members = start
            .roster
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let member_index = index as u32;
                let member_id = collection_member_id(&workflow_id, member_index, value);
                CollectionMemberRecord {
                    child_entity_id: member_id.clone(),
                    member_id,
                    member_index,
                    member_value: value.clone(),
                    status: CollectionMemberStatus::Pending,
                    admission_control_epoch: None,
                    terminal_control_epoch: None,
                    attempts: 0,
                    delivery_id: None,
                    cancellation_delivery_id: None,
                    delivery_status: None,
                    receipt: None,
                    failure_class: None,
                }
            })
            .collect::<Vec<_>>();
        let intent = CollectionStartIntentV1 {
            version: COLLECTION_LEDGER_VERSION,
            workflow_id: workflow_id.clone(),
            start: start.clone(),
        };
        let record = Self {
            version: COLLECTION_LEDGER_VERSION,
            workflow_id,
            tenant: start.tenant,
            source_entity_type: start.source_entity_type,
            source_entity_id: start.source_entity_id,
            declaration_name: start.declaration_name,
            source_action: start.source_action,
            source_sequence: start.source_sequence,
            schema_digest: start.schema_digest,
            schema_pin: start.schema_pin,
            original_authority: start.authority,
            execution_actions: None,
            timeout_binding: None,
            sealed_roster: start.roster,
            budgets,
            next_undispatched_index: 0,
            control_epoch: 0,
            status: CollectionWorkflowStatus::Running,
            requested_outcome: None,
            terminal_classification: None,
            join_status: CollectionJoinStatus::Pending,
            join_delivery_id: None,
            counts: CollectionWorkflowCounts {
                pending: members.len() as u16,
                ..CollectionWorkflowCounts::default()
            },
            total_attempts: 0,
            members,
            last_control_id: None,
            control_source_action: None,
            control_source_sequence: None,
            control_authority: None,
            control_schema_pin: None,
            control_timeout_delivery_id: None,
        };
        record.validate()?;
        Ok((intent, record))
    }

    /// Bind the verified, schema-pinned action contract before activation.
    pub(crate) fn bind_execution_actions(
        &mut self,
        actions: CollectionDeliveryActions,
    ) -> Result<CollectionMutationOutcome, String> {
        if let Some(existing) = &self.execution_actions {
            return if existing == &actions {
                Ok(CollectionMutationOutcome::Replayed)
            } else {
                Err("workflow execution actions are already bound".to_string())
            };
        }
        if [
            actions.member_entity.as_str(),
            actions.member_action.as_str(),
            actions.member_cancel_action.as_str(),
            actions.timeout_action.as_str(),
            actions.on_success.as_str(),
            actions.on_partial_failure.as_str(),
            actions.on_failure.as_str(),
            actions.on_cancelled.as_str(),
            actions.on_timed_out.as_str(),
        ]
        .iter()
        .any(|value| value.is_empty())
        {
            return Err("workflow execution action names must be non-empty".to_string());
        }
        self.execution_actions = Some(actions);
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Bind the exact generated ADR-0178 timeout intent before activation.
    pub(crate) fn bind_timeout(
        &mut self,
        binding: CollectionTimeoutBinding,
    ) -> Result<CollectionMutationOutcome, String> {
        if binding.delivery_id.is_empty()
            || binding.timeout_action.is_empty()
            || binding.state.is_empty()
            || binding.declaration_id.is_empty()
            || binding.clock_sequence != self.source_sequence
            || binding.schema_digest != self.schema_digest
        {
            return Err("collection timeout binding does not match its start clock".to_string());
        }
        if let Some(existing) = &self.timeout_binding {
            return if existing == &binding {
                Ok(CollectionMutationOutcome::Replayed)
            } else {
                Err("collection timeout clock is already bound".to_string())
            };
        }
        self.timeout_binding = Some(binding);
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Admit the exact next roster member under the current control epoch.
    pub(crate) fn admit_member(
        &mut self,
        member_index: u16,
        delivery_id: String,
        expected_control_epoch: u64,
    ) -> Result<CollectionMutationOutcome, String> {
        self.validate()?;
        if self.status != CollectionWorkflowStatus::Running {
            return Err("workflow is not running".to_string());
        }
        if expected_control_epoch != self.control_epoch {
            return Err("stale collection control epoch".to_string());
        }
        if member_index != self.next_undispatched_index {
            let prior = self.members.get(usize::from(member_index));
            if prior.is_some_and(|member| {
                member.status == CollectionMemberStatus::InFlight
                    && member.delivery_id.as_deref() == Some(delivery_id.as_str())
            }) {
                return Ok(CollectionMutationOutcome::Replayed);
            }
            return Err("member admission is not at the sealed roster cursor".to_string());
        }
        if self.counts.in_flight >= u16::from(self.budgets.max_concurrency) {
            return Err("collection concurrency window is full".to_string());
        }
        let member = self
            .members
            .get_mut(usize::from(member_index))
            .ok_or_else(|| "member index is outside the sealed roster".to_string())?;
        if member.status != CollectionMemberStatus::Pending {
            return Err("only a pending member can be admitted".to_string());
        }
        if delivery_id.is_empty() {
            return Err("delivery_id must be non-empty".to_string());
        }
        member.status = CollectionMemberStatus::InFlight;
        member.admission_control_epoch = Some(self.control_epoch);
        member.delivery_id = Some(delivery_id);
        member.delivery_status = Some(ReactionDeliveryStatus::Pending);
        self.next_undispatched_index += 1;
        self.recount();
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Record a target receipt without treating outstanding descendants as success.
    pub(crate) fn record_member_receipt(
        &mut self,
        member_id: &str,
        delivery_id: &str,
        control_epoch: u64,
        attempts: u8,
        receipt: CollectionMemberReceipt,
    ) -> Result<CollectionMutationOutcome, String> {
        if control_epoch != self.control_epoch {
            return Err("stale collection control epoch".to_string());
        }
        if attempts == 0 || attempts > self.budgets.max_attempts {
            return Err("member receipt attempts are outside the workflow budget".to_string());
        }
        if receipt.delivery_id != delivery_id || receipt.fencing_token == 0 {
            return Err("member receipt does not match its fenced delivery".to_string());
        }
        let member = self
            .members
            .iter_mut()
            .find(|member| member.member_id == member_id)
            .ok_or_else(|| "member receipt does not belong to this workflow".to_string())?;
        if member.status != CollectionMemberStatus::InFlight
            || member.delivery_id.as_deref() != Some(delivery_id)
            || member.admission_control_epoch != Some(control_epoch)
        {
            return Err("member receipt does not match an admitted delivery".to_string());
        }
        if let Some(committed) = &member.receipt {
            return if committed == &receipt && member.attempts == attempts {
                Ok(CollectionMutationOutcome::Replayed)
            } else {
                Err("conflicting receipt evidence for collection member".to_string())
            };
        }
        if attempts < member.attempts {
            return Err("member receipt cannot reduce attempt accounting".to_string());
        }
        member.attempts = attempts;
        member.delivery_status = Some(ReactionDeliveryStatus::Dispatching);
        member.receipt = Some(receipt);
        self.recount();
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Apply one fenced terminal delivery outcome exactly once.
    pub(crate) fn record_member_terminal(
        &mut self,
        evidence: CollectionMemberTerminalEvidence,
    ) -> Result<CollectionMutationOutcome, String> {
        if !evidence.status.is_terminal() {
            return Err("member evidence must be terminal".to_string());
        }
        if !matches!(
            evidence.status,
            CollectionMemberStatus::Succeeded | CollectionMemberStatus::Failed
        ) {
            return Err(
                "controlled terminal members require collection-control evidence".to_string(),
            );
        }
        if evidence.control_epoch != self.control_epoch {
            return Err("stale collection control epoch".to_string());
        }
        if evidence.attempts == 0 || evidence.attempts > self.budgets.max_attempts {
            return Err("member attempts exceed the workflow budget".to_string());
        }
        if !delivery_status_is_terminal(evidence.delivery_status) {
            return Err("member terminal evidence has a non-terminal delivery status".to_string());
        }
        if evidence.status == CollectionMemberStatus::Failed
            && evidence.delivery_status == ReactionDeliveryStatus::Succeeded
        {
            return Err("failed member cannot carry successful delivery evidence".to_string());
        }
        let member = self
            .members
            .iter_mut()
            .find(|member| member.member_id == evidence.member_id)
            .ok_or_else(|| "member evidence does not belong to this workflow".to_string())?;
        if member.status.is_terminal() {
            let matches = member.status == evidence.status
                && member.attempts == evidence.attempts
                && member.delivery_id == evidence.delivery_id
                && member.delivery_status == Some(evidence.delivery_status)
                && member.receipt == evidence.receipt
                && member.failure_class == evidence.failure_class
                && member.terminal_control_epoch == Some(evidence.control_epoch);
            return if matches {
                Ok(CollectionMutationOutcome::Replayed)
            } else {
                Err("conflicting terminal evidence for collection member".to_string())
            };
        }
        if member.status != CollectionMemberStatus::InFlight {
            return Err("only an admitted member can receive delivery evidence".to_string());
        }
        let Some(delivery_id) = evidence.delivery_id.as_deref() else {
            return Err("member terminal evidence requires a delivery identity".to_string());
        };
        if member.delivery_id.as_deref() != Some(delivery_id) {
            return Err("member terminal evidence names a different delivery".to_string());
        }
        if evidence.attempts < member.attempts {
            return Err("member terminal evidence cannot reduce attempt accounting".to_string());
        }
        if let Some(receipt) = &evidence.receipt
            && (receipt.delivery_id != delivery_id || receipt.fencing_token == 0)
        {
            return Err("member terminal receipt does not match its fenced delivery".to_string());
        }
        if evidence.status == CollectionMemberStatus::Succeeded
            && (evidence.delivery_status != ReactionDeliveryStatus::Succeeded
                || evidence.receipt.is_none()
                || member.receipt != evidence.receipt)
        {
            return Err("successful member evidence requires the committed receipt".to_string());
        }
        if evidence.status == CollectionMemberStatus::Failed && evidence.failure_class.is_none() {
            return Err("failed member evidence requires a sanitized failure class".to_string());
        }
        member.status = evidence.status;
        member.terminal_control_epoch = Some(evidence.control_epoch);
        member.attempts = evidence.attempts;
        member.delivery_status = Some(evidence.delivery_status);
        member.receipt = evidence.receipt;
        member.failure_class = evidence.failure_class;
        self.recount();
        self.classify_if_complete();
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Bind one cancellation delivery to a receipted active child.
    pub(crate) fn begin_member_cancellation(
        &mut self,
        member_id: &str,
        delivery_id: String,
        control_epoch: u64,
    ) -> Result<CollectionMutationOutcome, String> {
        if control_epoch != self.control_epoch || self.requested_outcome.is_none() {
            return Err("stale or absent collection control fence".to_string());
        }
        let member = self
            .members
            .iter_mut()
            .find(|member| member.member_id == member_id)
            .ok_or_else(|| "cancellation member does not belong to this workflow".to_string())?;
        if member.status != CollectionMemberStatus::InFlight || member.receipt.is_none() {
            return Err("only a receipted active member can be cancelled".to_string());
        }
        if let Some(existing) = &member.cancellation_delivery_id {
            return if existing == &delivery_id {
                Ok(CollectionMutationOutcome::Replayed)
            } else {
                Err("member has a conflicting cancellation delivery".to_string())
            };
        }
        if delivery_id.is_empty() {
            return Err("cancellation delivery identity must be non-empty".to_string());
        }
        member.cancellation_delivery_id = Some(delivery_id);
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    /// Apply one fenced cancellation delivery result exactly once.
    pub(crate) fn record_member_controlled_terminal(
        &mut self,
        member_id: &str,
        delivery_id: &str,
        control_epoch: u64,
        delivery_status: ReactionDeliveryStatus,
        matching_receipt: bool,
    ) -> Result<CollectionMutationOutcome, String> {
        if control_epoch != self.control_epoch || !delivery_status_is_terminal(delivery_status) {
            return Err(
                "controlled terminal evidence has a stale fence or active delivery".to_string(),
            );
        }
        let requested = self
            .requested_outcome
            .ok_or_else(|| "workflow has no control request".to_string())?;
        let controlled_terminal = match requested {
            CollectionRequestedOutcome::Cancelled => CollectionMemberStatus::Cancelled,
            CollectionRequestedOutcome::TimedOut => CollectionMemberStatus::TimedOut,
        };
        let terminal = if delivery_status == ReactionDeliveryStatus::Succeeded
            || (delivery_status == ReactionDeliveryStatus::Skipped && matching_receipt)
        {
            controlled_terminal
        } else {
            CollectionMemberStatus::Failed
        };
        let member = self
            .members
            .iter_mut()
            .find(|member| member.member_id == member_id)
            .ok_or_else(|| "controlled member does not belong to this workflow".to_string())?;
        if member.status.is_terminal() {
            return if member.status == terminal
                && member.cancellation_delivery_id.as_deref() == Some(delivery_id)
                && member.terminal_control_epoch == Some(control_epoch)
            {
                Ok(CollectionMutationOutcome::Replayed)
            } else {
                Err("conflicting controlled terminal evidence".to_string())
            };
        }
        if member.status != CollectionMemberStatus::InFlight
            || member.receipt.is_none()
            || member.cancellation_delivery_id.as_deref() != Some(delivery_id)
        {
            return Err(
                "controlled terminal evidence is not bound to a receipted child".to_string(),
            );
        }
        member.status = terminal;
        member.terminal_control_epoch = Some(control_epoch);
        member.delivery_status = Some(delivery_status);
        member.failure_class = (terminal == CollectionMemberStatus::Failed)
            .then_some(CollectionFailureClass::CancellationFailed);
        self.recount();
        self.classify_if_complete();
        self.validate()?;
        Ok(CollectionMutationOutcome::Applied)
    }

    fn classify_if_complete(&mut self) {
        if self.counts.terminal() != self.members.len() as u16 {
            return;
        }
        let classification = match self.requested_outcome {
            Some(CollectionRequestedOutcome::Cancelled) => CollectionWorkflowStatus::Cancelled,
            Some(CollectionRequestedOutcome::TimedOut) => CollectionWorkflowStatus::TimedOut,
            None if self.counts.succeeded == self.members.len() as u16 => {
                CollectionWorkflowStatus::Succeeded
            }
            None if self.counts.succeeded > 0 => CollectionWorkflowStatus::PartiallyFailed,
            None => CollectionWorkflowStatus::Failed,
        };
        self.status = classification;
        self.terminal_classification = Some(classification);
    }
}

fn delivery_status_is_terminal(status: ReactionDeliveryStatus) -> bool {
    matches!(
        status,
        ReactionDeliveryStatus::Succeeded
            | ReactionDeliveryStatus::Skipped
            | ReactionDeliveryStatus::DroppedAllowed
            | ReactionDeliveryStatus::Rejected
            | ReactionDeliveryStatus::DeadLettered
    )
}
