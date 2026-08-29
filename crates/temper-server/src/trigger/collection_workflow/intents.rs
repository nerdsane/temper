//! Deterministic durable delivery intents for collection execution.

use std::collections::BTreeMap;

use temper_runtime::scheduler::sim_now;

use super::*;
use crate::trigger::delivery::{DeliveryKind, PersistedReactionIntent, stable_delivery_id};
use crate::trigger::types::{ReactionRule, ReactionTarget, ReactionTrigger, TargetResolver};

/// Closed action names bound from one verified declaration.
pub(crate) struct CollectionExecutionActions<'a> {
    pub(crate) member_entity: &'a str,
    pub(crate) member_action: &'a str,
    pub(crate) member_cancel_action: &'a str,
    pub(crate) timeout_action: &'a str,
    pub(crate) on_success: &'a str,
    pub(crate) on_partial_failure: &'a str,
    pub(crate) on_failure: &'a str,
    pub(crate) on_cancelled: &'a str,
    pub(crate) on_timed_out: &'a str,
}

impl CollectionExecutionActions<'_> {
    pub(super) fn owned(&self) -> CollectionDeliveryActions {
        CollectionDeliveryActions {
            member_entity: self.member_entity.to_string(),
            member_action: self.member_action.to_string(),
            member_cancel_action: self.member_cancel_action.to_string(),
            timeout_action: self.timeout_action.to_string(),
            on_success: self.on_success.to_string(),
            on_partial_failure: self.on_partial_failure.to_string(),
            on_failure: self.on_failure.to_string(),
            on_cancelled: self.on_cancelled.to_string(),
            on_timed_out: self.on_timed_out.to_string(),
        }
    }

    fn join(&self, classification: CollectionWorkflowStatus) -> Result<&str, String> {
        match classification {
            CollectionWorkflowStatus::Succeeded => Ok(self.on_success),
            CollectionWorkflowStatus::PartiallyFailed => Ok(self.on_partial_failure),
            CollectionWorkflowStatus::Failed => Ok(self.on_failure),
            CollectionWorkflowStatus::Cancelled => Ok(self.on_cancelled),
            CollectionWorkflowStatus::TimedOut => Ok(self.on_timed_out),
            _ => Err("join action requested before terminal classification".to_string()),
        }
    }
}

impl CollectionDeliveryActions {
    pub(super) fn borrowed(&self) -> CollectionExecutionActions<'_> {
        CollectionExecutionActions {
            member_entity: &self.member_entity,
            member_action: &self.member_action,
            member_cancel_action: &self.member_cancel_action,
            timeout_action: &self.timeout_action,
            on_success: &self.on_success,
            on_partial_failure: &self.on_partial_failure,
            on_failure: &self.on_failure,
            on_cancelled: &self.on_cancelled,
            on_timed_out: &self.on_timed_out,
        }
    }
}

struct CollectionIntentDraft<'a> {
    delivery_id: String,
    trigger_name: String,
    trigger_index: usize,
    target_entity: &'a str,
    target_action: &'a str,
    target_entity_id: String,
    params: serde_json::Value,
    kind: DeliveryKind,
    collection: CollectionDeliveryContext,
}

/// Admit the next deterministic concurrency window and return its durable
/// member intents. The record mutation and intents must be persisted together.
pub(crate) fn admit_collection_window(
    record: &mut CollectionWorkflowRecordV1,
    workflow_sequence: u64,
    actions: &CollectionExecutionActions<'_>,
) -> Result<Vec<PersistedReactionIntent>, String> {
    let available = u16::from(record.budgets.max_concurrency) - record.counts.in_flight;
    let count = available.min(record.counts.pending);
    let mut intents = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let index = record.next_undispatched_index;
        let member = record.members[usize::from(index)].clone();
        let trigger = format!("collection-member:{}", member.member_id);
        let delivery_id = delivery_id(record, workflow_sequence, &trigger, usize::from(index));
        record.admit_member(index, delivery_id.clone(), record.control_epoch)?;
        intents.push(intent(
            record,
            workflow_sequence,
            CollectionIntentDraft {
                delivery_id,
                trigger_name: trigger,
                trigger_index: usize::from(index),
                target_entity: actions.member_entity,
                target_action: actions.member_action,
                target_entity_id: member.child_entity_id,
                params: serde_json::json!({
                    "workflow_id": record.workflow_id,
                    "member_id": member.member_id,
                    "member_value": member.member_value,
                    "source_entity_id": record.source_entity_id,
                    "member_index": member.member_index,
                }),
                kind: DeliveryKind::CollectionMember,
                collection: CollectionDeliveryContext {
                    workflow_id: record.workflow_id.clone(),
                    member_id: Some(member.member_id),
                    control_epoch: record.control_epoch,
                    role: CollectionDeliveryRole::Member,
                    terminal_classification: None,
                    actions: actions.owned(),
                    max_attempts: record.budgets.max_attempts,
                    attempts: 0,
                    execution_deadline: record.timeout_binding.as_ref().map(|clock| clock.deadline),
                },
            },
        )?);
    }
    Ok(intents)
}

/// Create cancellation deliveries only for children whose member receipt
/// proves that their target action committed before control won.
pub(crate) fn collection_cancellation_intents(
    record: &mut CollectionWorkflowRecordV1,
    workflow_sequence: u64,
    actions: &CollectionExecutionActions<'_>,
) -> Result<Vec<PersistedReactionIntent>, String> {
    let requested = record
        .requested_outcome
        .ok_or_else(|| "workflow has no control request".to_string())?;
    let candidates = record
        .members
        .iter()
        .filter(|member| {
            member.status == CollectionMemberStatus::InFlight
                && member.receipt.is_some()
                && member.cancellation_delivery_id.is_none()
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut intents = Vec::with_capacity(candidates.len());
    for member in candidates {
        let trigger = format!(
            "collection-cancel:{}:{}",
            record.control_epoch, member.member_id
        );
        let delivery_id = delivery_id(
            record,
            workflow_sequence,
            &trigger,
            member.member_index as usize,
        );
        record.begin_member_cancellation(
            &member.member_id,
            delivery_id.clone(),
            record.control_epoch,
        )?;
        intents.push(intent(record, workflow_sequence, CollectionIntentDraft {
            delivery_id,
            trigger_name: trigger,
            trigger_index: member.member_index as usize,
            target_entity: actions.member_entity,
            target_action: actions.member_cancel_action,
            target_entity_id: member.child_entity_id,
            params: serde_json::json!({
                "workflow_id": record.workflow_id,
                "member_id": member.member_id,
                "member_value": member.member_value,
                "source_entity_id": record.source_entity_id,
                "member_index": member.member_index,
                "requested_outcome": match requested { CollectionRequestedOutcome::Cancelled => "cancelled", CollectionRequestedOutcome::TimedOut => "timed_out" },
            }),
            kind: DeliveryKind::CollectionCancellation,
            collection: CollectionDeliveryContext {
                workflow_id: record.workflow_id.clone(), member_id: Some(member.member_id),
                control_epoch: record.control_epoch, role: CollectionDeliveryRole::Cancellation,
                terminal_classification: None,
                actions: actions.owned(),
                max_attempts: record.budgets.max_attempts,
                attempts: 0,
                execution_deadline: record.timeout_binding.as_ref().map(|clock| clock.deadline),
            },
        })?);
    }
    Ok(intents)
}

/// Create the sole classified join intent for a terminal workflow.
pub(crate) fn collection_join_intent(
    record: &mut CollectionWorkflowRecordV1,
    workflow_sequence: u64,
    actions: &CollectionExecutionActions<'_>,
) -> Result<Option<PersistedReactionIntent>, String> {
    let Some(classification) = record.terminal_classification else {
        return Ok(None);
    };
    if record.join_delivery_id.is_some() {
        return Ok(None);
    }
    let trigger = format!("collection-join:{}", record.workflow_id);
    let delivery_id = delivery_id(record, workflow_sequence, &trigger, 0);
    record.begin_join(delivery_id.clone())?;
    Ok(Some(intent(
        record,
        workflow_sequence,
        CollectionIntentDraft {
            delivery_id,
            trigger_name: trigger,
            trigger_index: 0,
            target_entity: &record.source_entity_type,
            target_action: actions.join(classification)?,
            target_entity_id: record.source_entity_id.clone(),
            params: serde_json::json!({
                "workflow_id": record.workflow_id,
                "total_members": record.members.len(),
                "succeeded_members": record.counts.succeeded,
                "failed_members": record.counts.failed,
                "cancelled_members": record.counts.cancelled,
                "timed_out_members": record.counts.timed_out,
            }),
            kind: DeliveryKind::CollectionJoin,
            collection: CollectionDeliveryContext {
                workflow_id: record.workflow_id.clone(),
                member_id: None,
                control_epoch: record.control_epoch,
                role: CollectionDeliveryRole::Join,
                terminal_classification: Some(classification),
                actions: actions.owned(),
                max_attempts: record.budgets.max_attempts,
                attempts: 0,
                execution_deadline: record.timeout_binding.as_ref().map(|clock| clock.deadline),
            },
        },
    )?))
}

fn delivery_id(
    record: &CollectionWorkflowRecordV1,
    sequence: u64,
    trigger: &str,
    index: usize,
) -> String {
    stable_delivery_id(
        &record.tenant,
        COLLECTION_WORKFLOW_ENTITY_TYPE,
        &record.workflow_id,
        "CollectionWorkflow::AdvancedV1",
        sequence + 1,
        trigger,
        index,
    )
}

fn intent(
    record: &CollectionWorkflowRecordV1,
    sequence: u64,
    draft: CollectionIntentDraft<'_>,
) -> Result<PersistedReactionIntent, String> {
    let CollectionIntentDraft {
        delivery_id,
        trigger_name,
        trigger_index,
        target_entity,
        target_action,
        target_entity_id,
        params,
        kind,
        collection,
    } = draft;
    let rule = ReactionRule {
        name: trigger_name.clone(),
        when: ReactionTrigger {
            entity_type: COLLECTION_WORKFLOW_ENTITY_TYPE.to_string(),
            action: Some("CollectionWorkflow::AdvancedV1".to_string()),
            to_state: None,
            guard: None,
        },
        then: ReactionTarget {
            entity_type: target_entity.to_string(),
            action: target_action.to_string(),
            params,
            params_from: BTreeMap::new(),
        },
        resolve_target: TargetResolver::Static {
            entity_id: target_entity_id.clone(),
        },
        principal: Some("wasm-runtime".to_string()),
        drop_ok: false,
    };
    let (authority, schema_pin) = match kind {
        DeliveryKind::CollectionCancellation => (
            record
                .control_authority
                .clone()
                .ok_or_else(|| "collection cancellation has no control authority".to_string())?,
            record.control_schema_pin.clone(),
        ),
        DeliveryKind::CollectionMember | DeliveryKind::CollectionJoin => {
            (record.original_authority.clone(), record.schema_pin.clone())
        }
        DeliveryKind::Reaction | DeliveryKind::StateTimeout => {
            return Err("collection builder received a foreign delivery kind".to_string());
        }
    };
    Ok(PersistedReactionIntent {
        kind,
        root_delivery_id: delivery_id.clone(),
        delivery_id,
        tenant: record.tenant.clone(),
        source_entity_type: COLLECTION_WORKFLOW_ENTITY_TYPE.to_string(),
        source_entity_id: record.workflow_id.clone(),
        source_action: "CollectionWorkflow::AdvancedV1".to_string(),
        source_sequence: sequence + 1,
        source_to_state: format!("{:?}", record.status),
        source_fields: serde_json::json!({"workflow_id": record.workflow_id, "control_epoch": record.control_epoch}),
        source_stream_descriptor: None,
        guard_passed: true,
        target_entity_id: Some(target_entity_id),
        trigger_name,
        trigger_index,
        depth: 0,
        rule: serde_json::to_value(rule).map_err(|error| error.to_string())?,
        authority,
        created_at: sim_now(),
        not_before: None,
        state_timeout: None,
        collection: Some(collection),
        schema_pin,
    })
}
