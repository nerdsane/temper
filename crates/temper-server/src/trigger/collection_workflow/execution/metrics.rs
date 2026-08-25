//! Post-commit collection workflow metric projections.

use super::super::{
    CollectionControlIntentV1, CollectionDeliveryContext, CollectionLedgerCommitOutcome,
    CollectionMemberStatus, CollectionRequestedOutcome, CollectionWorkflowRecordV1,
};

pub(super) fn record_start_commit(
    outcome: &CollectionLedgerCommitOutcome,
    record: &CollectionWorkflowRecordV1,
) {
    if matches!(outcome, CollectionLedgerCommitOutcome::Committed(_)) {
        crate::runtime_metrics::record_collection_workflow_event("start", "running");
        crate::runtime_metrics::record_collection_active_window(record.counts.in_flight);
    }
}

pub(super) fn record_control_commit(
    outcome: &CollectionLedgerCommitOutcome,
    intent: &CollectionControlIntentV1,
    record: &CollectionWorkflowRecordV1,
) {
    if !matches!(outcome, CollectionLedgerCommitOutcome::Committed(_))
        || record.last_control_id.as_deref() != Some(intent.control_id.as_str())
    {
        return;
    }
    let requested = match record.requested_outcome {
        Some(CollectionRequestedOutcome::Cancelled) => "cancelled",
        Some(CollectionRequestedOutcome::TimedOut) => "timed_out",
        None => "ignored",
    };
    crate::runtime_metrics::record_collection_workflow_event("control", requested);
    crate::runtime_metrics::record_collection_active_window(record.counts.in_flight);
    for member in record.members.iter().filter(|member| {
        matches!(
            member.status,
            CollectionMemberStatus::Cancelled | CollectionMemberStatus::TimedOut
        )
    }) {
        crate::runtime_metrics::record_collection_member_outcome(member.status);
    }
    if record.status.is_terminal() {
        crate::runtime_metrics::record_collection_terminal_classification(record.status);
    }
}

pub(super) fn record_terminal_commit(
    was_terminal: bool,
    prior_member_status: Option<CollectionMemberStatus>,
    context: &CollectionDeliveryContext,
    record: &CollectionWorkflowRecordV1,
) {
    crate::runtime_metrics::record_collection_active_window(record.counts.in_flight);
    if let Some(member_id) = context.member_id.as_deref()
        && let Some(member) = record
            .members
            .iter()
            .find(|member| member.member_id == member_id)
        && Some(member.status) != prior_member_status
        && member.status.is_terminal()
    {
        crate::runtime_metrics::record_collection_member_outcome(member.status);
    }
    if !was_terminal && record.status.is_terminal() {
        crate::runtime_metrics::record_collection_terminal_classification(record.status);
    }
}
