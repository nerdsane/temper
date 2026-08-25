//! Redacted response types for collection workflow observation.

use serde::Serialize;

use crate::trigger::collection_workflow::{
    CollectionFailureClass, CollectionJoinStatus, CollectionMemberRecord,
    CollectionRequestedOutcome, CollectionWorkflowBudgets, CollectionWorkflowCounts,
    CollectionWorkflowRecordV1, CollectionWorkflowStatus,
};
use crate::trigger::delivery::ReactionDeliveryRecord;
use crate::trigger::delivery::ReactionDeliveryStatus;

/// Bounded list response with an optional opaque continuation.
#[derive(Debug, Serialize)]
pub(crate) struct WorkflowListResponse {
    pub(super) value: Vec<WorkflowSummary>,
    pub(super) next_cursor: Option<String>,
}

/// Redacted workflow detail containing at most the v1 member budget.
#[derive(Debug, Serialize)]
pub(crate) struct WorkflowDetailResponse {
    #[serde(flatten)]
    pub(super) summary: WorkflowSummary,
    pub(super) members: Vec<MemberView>,
}

/// Bounded redacted member page.
#[derive(Debug, Serialize)]
pub(crate) struct MemberPageResponse {
    pub(super) value: Vec<MemberView>,
    pub(super) next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct SourceIdentity {
    entity_type: String,
    entity_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct WorkflowSummary {
    workflow_id: String,
    declaration: String,
    source: SourceIdentity,
    schema_digest: String,
    status: CollectionWorkflowStatus,
    requested_outcome: Option<CollectionRequestedOutcome>,
    join_status: CollectionJoinStatus,
    budgets: CollectionWorkflowBudgets,
    sealed_member_count: usize,
    counts: CollectionWorkflowCounts,
    total_attempts: u32,
    oldest_active_age_ms: Option<u64>,
    terminal_reason: Option<CollectionFailureClass>,
    join_delivery_reason: Option<&'static str>,
}

impl WorkflowSummary {
    pub(super) fn from_record(
        record: &CollectionWorkflowRecordV1,
        total_attempts: u32,
        oldest_active_age_ms: Option<u64>,
    ) -> Self {
        let terminal_reason = record
            .members
            .iter()
            .filter_map(|member| member.failure_class)
            .next();
        let join_delivery_reason = match record.join_status {
            CollectionJoinStatus::DeliveryFailed => Some("delivery_failed"),
            CollectionJoinStatus::SupersededByNewWorkflow => Some("superseded_by_new_workflow"),
            CollectionJoinStatus::Pending
            | CollectionJoinStatus::InFlight
            | CollectionJoinStatus::Delivered => None,
        };
        Self {
            workflow_id: record.workflow_id.clone(),
            declaration: record.declaration_name.clone(),
            source: SourceIdentity {
                entity_type: record.source_entity_type.clone(),
                entity_id: record.source_entity_id.clone(),
            },
            schema_digest: record.schema_digest.clone(),
            status: record.status,
            requested_outcome: record.requested_outcome,
            join_status: record.join_status,
            budgets: record.budgets,
            sealed_member_count: record.members.len(),
            counts: record.counts,
            total_attempts,
            oldest_active_age_ms,
            terminal_reason,
            join_delivery_reason,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct MemberView {
    member_id: String,
    member_index: u32,
    status: crate::trigger::collection_workflow::CollectionMemberStatus,
    attempts: u32,
    delivery_class: Option<ReactionDeliveryStatus>,
    failure_class: Option<CollectionFailureClass>,
}

impl From<&CollectionMemberRecord> for MemberView {
    fn from(member: &CollectionMemberRecord) -> Self {
        Self {
            member_id: member.member_id.clone(),
            member_index: member.member_index,
            status: member.status,
            attempts: u32::from(member.attempts),
            delivery_class: member.delivery_status,
            failure_class: member.failure_class,
        }
    }
}

impl MemberView {
    pub(super) fn from_delivery(
        member: &CollectionMemberRecord,
        delivery: &ReactionDeliveryRecord,
        attempts: u32,
    ) -> Self {
        Self {
            member_id: member.member_id.clone(),
            member_index: member.member_index,
            status: member.status,
            attempts,
            delivery_class: Some(delivery.status),
            failure_class: member.failure_class,
        }
    }

    pub(super) const fn attempts(&self) -> u32 {
        self.attempts
    }

    pub(super) const fn member_index(&self) -> u32 {
        self.member_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::collection_workflow::{
        CollectionWorkflowRecordV1, CollectionWorkflowStart,
    };

    #[test]
    fn summary_and_member_views_cannot_serialize_private_ledger_fields() {
        let (_, record) = CollectionWorkflowRecordV1::start(CollectionWorkflowStart {
            tenant: "tenant-a".into(),
            source_entity_type: "Batch".into(),
            source_entity_id: "source-1".into(),
            declaration_name: "run".into(),
            source_action: "Start".into(),
            source_sequence: 1,
            schema_digest: "schema-1".into(),
            schema_pin: None,
            authority: serde_json::json!({"principal": {"id": "private-principal"}}),
            roster: vec!["private-member-value".into()],
            budgets: CollectionWorkflowBudgets {
                max_members: 1,
                max_concurrency: 1,
                max_attempts: 1,
            },
        })
        .expect("valid workflow");
        let detail = WorkflowDetailResponse {
            summary: WorkflowSummary::from_record(&record, record.total_attempts, None),
            members: record.members.iter().map(MemberView::from).collect(),
        };
        let encoded = serde_json::to_string(&detail).expect("serializable response");
        for secret in [
            "private-member-value",
            "sealed_roster",
            "original_authority",
            "control_authority",
            "receipt",
            "child_entity_id",
        ] {
            assert!(!encoded.contains(secret), "response exposed {secret}");
        }
    }
}
