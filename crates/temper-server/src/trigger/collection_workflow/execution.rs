//! Target-commit fencing for private collection deliveries.

use super::{
    CollectionDeliveryContext, CollectionDeliveryRole, CollectionJoinStatus,
    CollectionMemberStatus, CollectionWorkflowStatus, load_collection_record,
};
use crate::storage::BoxedEventStore;
use crate::trigger::delivery::{ReactionDeliveryRecord, ReactionDeliveryStatus};

mod metrics;
mod outcome;
mod receipt;
mod timeout_binding;

/// Bind execution to the pinned declaration and create the initial bounded
/// member window for the workflow's first journal event.
pub(crate) fn activate_start(
    record: &mut super::CollectionWorkflowRecordV1,
    workflow_sequence: u64,
    actions: &super::CollectionExecutionActions<'_>,
) -> Result<Vec<crate::trigger::delivery::PersistedReactionIntent>, String> {
    record.bind_execution_actions(actions.owned())?;
    super::admit_collection_window(record, workflow_sequence, actions)
}

/// Atomically commit the source start, activated workflow, and initial member
/// intents so recovery never sees a running workflow without its first window.
pub(crate) async fn commit_activated_start(
    store: &BoxedEventStore,
    source_append: temper_runtime::persistence::PersistenceAppend,
    intent: &super::CollectionStartIntentV1,
    record: &mut super::CollectionWorkflowRecordV1,
    actions: &super::CollectionExecutionActions<'_>,
) -> Result<super::CollectionLedgerCommitOutcome, String> {
    let mut superseded_append = None;
    if let Some(active_workflow_id) = super::active_source_workflow_id(store, record)
        .await
        .map_err(|error| error.to_string())?
        && active_workflow_id != record.workflow_id
        && let Some((mut active, active_sequence)) =
            load_collection_record(store, &record.tenant, &active_workflow_id)
                .await
                .map_err(|error| error.to_string())?
    {
        match active.join_status {
            CollectionJoinStatus::InFlight | CollectionJoinStatus::DeliveryFailed => {
                active.supersede_join()?;
                superseded_append = Some(
                    super::workflow_append(
                        &active,
                        active_sequence,
                        "CollectionWorkflow::JoinSupersededV1",
                    )
                    .map_err(|error| error.to_string())?,
                );
            }
            CollectionJoinStatus::Delivered | CollectionJoinStatus::SupersededByNewWorkflow => {}
            CollectionJoinStatus::Pending => {
                return Err(
                    "a newer collection cannot start before the active workflow joins".to_string(),
                );
            }
        }
    }
    timeout_binding::bind_timeout_from_source(record, &source_append, actions.timeout_action)?;
    let intents = activate_start(record, 0, actions)?;
    let outcome = super::commit_collection_start_with_intents(
        store,
        source_append,
        intent,
        record,
        &intents,
        superseded_append.as_slice(),
    )
    .await
    .map_err(|error| error.to_string())?;
    metrics::record_start_commit(&outcome, record);
    Ok(outcome)
}

/// Atomically commit first-writer control and every cancellation/join intent
/// derivable from that fenced snapshot.
pub(crate) async fn commit_controlled(
    store: &BoxedEventStore,
    source_append: temper_runtime::persistence::PersistenceAppend,
    intent: &super::CollectionControlIntentV1,
    expected_workflow_sequence: u64,
    record: &mut super::CollectionWorkflowRecordV1,
) -> Result<super::CollectionLedgerCommitOutcome, String> {
    let intents = recover_progress(record, expected_workflow_sequence)?;
    let mut delivery_appends = Vec::new();
    for member in record.members.iter().filter(|member| {
        member.receipt.is_none()
            && member.delivery_status == Some(ReactionDeliveryStatus::Skipped)
            && member.delivery_id.is_some()
    }) {
        let delivery_id = member.delivery_id.as_deref().expect("filtered as present");
        let delivery_intent = super::find_collection_intent(store, record, delivery_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "controlled member delivery intent is missing".to_string())?;
        let (mut delivery, delivery_sequence) =
            crate::trigger::delivery::load_delivery_record(store, delivery_intent)
                .await
                .map_err(|error| error.to_string())?;
        delivery.status = ReactionDeliveryStatus::Skipped;
        delivery.lease_expires_at = None;
        delivery.next_attempt_at = None;
        delivery.last_error = Some("collection control fenced delivery before receipt".to_string());
        delivery_appends.push(
            crate::trigger::delivery::delivery_record_append(delivery_sequence, &delivery)
                .map_err(|error| error.to_string())?,
        );
    }
    let outcome = super::commit_collection_control_with_intents(
        store,
        source_append,
        intent,
        expected_workflow_sequence,
        record,
        &intents,
        &delivery_appends,
    )
    .await
    .map_err(|error| error.to_string())?;
    metrics::record_control_commit(&outcome, intent, record);
    Ok(outcome)
}

/// Deterministically reconstruct the next missing execution intents from a
/// replayed snapshot. Stable identities make repeated recovery a no-op.
pub(crate) fn recover_progress(
    record: &mut super::CollectionWorkflowRecordV1,
    workflow_sequence: u64,
) -> Result<Vec<crate::trigger::delivery::PersistedReactionIntent>, String> {
    let actions = record
        .execution_actions
        .clone()
        .ok_or_else(|| "collection workflow execution is not activated".to_string())?;
    continuation_intents(record, workflow_sequence, &actions)
}

fn validate_loaded_fence(
    record: &super::CollectionWorkflowRecordV1,
    delivery_id: &str,
    context: &CollectionDeliveryContext,
) -> Result<(), String> {
    if record.control_epoch != context.control_epoch {
        return Err("stale collection control epoch at target commit".to_string());
    }
    match context.role {
        CollectionDeliveryRole::Member => {
            if record.status != CollectionWorkflowStatus::Running {
                return Err("collection member admission was fenced by control".to_string());
            }
            let member = member(&record.members, context)?;
            if member.status != CollectionMemberStatus::InFlight
                || member.admission_control_epoch != Some(context.control_epoch)
                || member.delivery_id.as_deref() != Some(delivery_id)
            {
                return Err("collection member delivery is not the active admission".to_string());
            }
        }
        CollectionDeliveryRole::Cancellation => {
            if record.requested_outcome.is_none() {
                return Err("collection cancellation has no committed control".to_string());
            }
            let member = member(&record.members, context)?;
            if member.status != CollectionMemberStatus::InFlight
                || member.receipt.is_none()
                || member.cancellation_delivery_id.as_deref() != Some(delivery_id)
            {
                return Err("collection cancellation is not bound to an active receipt".to_string());
            }
        }
        CollectionDeliveryRole::Join => {
            if !record.status.is_terminal()
                || record.terminal_classification != context.terminal_classification
                || record.join_status != CollectionJoinStatus::InFlight
                || record.join_delivery_id.as_deref() != Some(delivery_id)
            {
                return Err("collection join no longer matches terminal classification".to_string());
            }
        }
        CollectionDeliveryRole::MemberDescendant => {
            let member = member(&record.members, context)?;
            if record.status != CollectionWorkflowStatus::Running
                || member.status != CollectionMemberStatus::InFlight
                || member.receipt.is_none()
            {
                return Err(
                    "collection member descendant was fenced by lifecycle change".to_string(),
                );
            }
        }
        CollectionDeliveryRole::CancellationDescendant => {
            let member = member(&record.members, context)?;
            if record.requested_outcome.is_none()
                || member.status != CollectionMemberStatus::InFlight
                || member.receipt.is_none()
                || member.cancellation_delivery_id.is_none()
            {
                return Err(
                    "collection cancellation descendant lost its active lineage".to_string()
                );
            }
        }
        CollectionDeliveryRole::JoinDescendant => {
            if !record.status.is_terminal()
                || record.terminal_classification != context.terminal_classification
                || record.join_status != CollectionJoinStatus::InFlight
            {
                return Err("collection join descendant lost its terminal fence".to_string());
            }
        }
    }
    Ok(())
}

/// Build the workflow side of the atomic target+fence commit. The returned
/// append carries the exact sequence read here; a concurrent control append
/// makes the whole target batch fail optimistic concurrency.
pub(crate) async fn target_fence_append(
    store: &BoxedEventStore,
    tenant: &str,
    receipt: &crate::trigger::delivery::ReactionReceipt,
) -> Result<temper_runtime::persistence::PersistenceAppend, String> {
    let context = receipt
        .collection
        .as_ref()
        .ok_or_else(|| "collection target commit is missing its workflow fence".to_string())?;
    let (mut record, sequence) = load_collection_record(store, tenant, &context.workflow_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "collection workflow journal is missing".to_string())?;
    validate_loaded_fence(&record, &receipt.delivery_id, context)?;
    if matches!(
        context.role,
        CollectionDeliveryRole::Join | CollectionDeliveryRole::JoinDescendant
    ) && super::active_source_workflow_id(store, &record)
        .await
        .map_err(|error| error.to_string())?
        .as_deref()
        != Some(record.workflow_id.as_str())
    {
        return Err("SupersededByNewWorkflow: collection join lost the source fence".to_string());
    }
    match context.role {
        CollectionDeliveryRole::Member => {
            let member_id = context
                .member_id
                .as_deref()
                .ok_or_else(|| "collection member target has no member identity".to_string())?;
            let member_receipt = super::CollectionMemberReceipt {
                delivery_id: receipt.delivery_id.clone(),
                fencing_token: receipt.fencing_token,
            };
            record.record_member_receipt(
                member_id,
                &receipt.delivery_id,
                context.control_epoch,
                context.attempts,
                member_receipt,
            )?;
        }
        CollectionDeliveryRole::Cancellation
        | CollectionDeliveryRole::Join
        | CollectionDeliveryRole::MemberDescendant
        | CollectionDeliveryRole::CancellationDescendant
        | CollectionDeliveryRole::JoinDescendant => {}
    }
    let append = super::workflow_append(&record, sequence, "CollectionWorkflow::TargetCommittedV1")
        .map_err(|error| error.to_string())?;
    Ok(append)
}

fn member<'a>(
    members: &'a [super::CollectionMemberRecord],
    context: &CollectionDeliveryContext,
) -> Result<&'a super::CollectionMemberRecord, String> {
    let member_id = context
        .member_id
        .as_deref()
        .ok_or_else(|| "collection member delivery has no member identity".to_string())?;
    members
        .iter()
        .find(|member| member.member_id == member_id)
        .ok_or_else(|| "collection delivery member is outside the sealed roster".to_string())
}

/// Fold a terminal delivery into its workflow and commit both journals in one
/// batch. Returns `Ok(false)` for ordinary non-collection deliveries.
pub(crate) async fn commit_terminal_delivery(
    store: &BoxedEventStore,
    expected_delivery_sequence: u64,
    delivery: &ReactionDeliveryRecord,
) -> Result<bool, String> {
    let Some(context) = delivery.intent.collection.as_ref() else {
        return Ok(false);
    };
    if context.role.is_descendant() {
        return Ok(false);
    }
    if !delivery.status.is_terminal() {
        return Err("collection delivery outcome is not terminal".to_string());
    }
    let matching_receipt = receipt::has_matching_target_receipt(store, delivery).await?;
    let (mut record, workflow_sequence) =
        load_collection_record(store, &delivery.intent.tenant, &context.workflow_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "collection workflow journal is missing".to_string())?;
    let was_terminal = record.status.is_terminal();
    let prior_member_status = context.member_id.as_deref().and_then(|member_id| {
        record
            .members
            .iter()
            .find(|member| member.member_id == member_id)
            .map(|member| member.status)
    });
    if context.role == CollectionDeliveryRole::Member
        && matching_receipt
        && let Some(member_id) = context.member_id.as_deref()
        && let Some(member) = record
            .members
            .iter()
            .find(|member| member.member_id == member_id)
        && member.delivery_id.as_deref() == Some(delivery.intent.delivery_id.as_str())
        && matches!(
            member.status,
            CollectionMemberStatus::Cancelled | CollectionMemberStatus::TimedOut
        )
    {
        crate::trigger::delivery::append_delivery_record(
            store,
            expected_delivery_sequence,
            delivery,
        )
        .await
        .map_err(|error| error.to_string())?;
        return Ok(true);
    }
    let join_is_superseded = matches!(
        context.role,
        CollectionDeliveryRole::Join | CollectionDeliveryRole::JoinDescendant
    ) && super::active_source_workflow_id(store, &record)
        .await
        .map_err(|error| error.to_string())?
        .as_deref()
        != Some(record.workflow_id.as_str());
    match context.role {
        CollectionDeliveryRole::Member => {
            let member_id = context
                .member_id
                .as_deref()
                .ok_or_else(|| "collection member outcome has no member identity".to_string())?;
            let attempts = u8::try_from(delivery.attempts)
                .map_err(|_| "collection member attempt count overflowed".to_string())?;
            if delivery.status == ReactionDeliveryStatus::Succeeded {
                let receipt = super::CollectionMemberReceipt {
                    delivery_id: delivery.intent.delivery_id.clone(),
                    fencing_token: delivery.fencing_token,
                };
                record.record_member_receipt(
                    member_id,
                    &delivery.intent.delivery_id,
                    context.control_epoch,
                    attempts,
                    receipt.clone(),
                )?;
                record.record_member_terminal(super::CollectionMemberTerminalEvidence {
                    member_id: member_id.to_string(),
                    control_epoch: context.control_epoch,
                    status: CollectionMemberStatus::Succeeded,
                    attempts,
                    delivery_id: Some(delivery.intent.delivery_id.clone()),
                    delivery_status: delivery.status,
                    receipt: Some(receipt),
                    failure_class: None,
                })?;
            } else {
                record.record_member_terminal(super::CollectionMemberTerminalEvidence {
                    member_id: member_id.to_string(),
                    control_epoch: context.control_epoch,
                    status: CollectionMemberStatus::Failed,
                    attempts,
                    delivery_id: Some(delivery.intent.delivery_id.clone()),
                    delivery_status: delivery.status,
                    receipt: None,
                    failure_class: Some(outcome::failure_class(delivery.status)),
                })?;
            }
        }
        CollectionDeliveryRole::Cancellation => {
            record.record_member_controlled_terminal(
                context.member_id.as_deref().ok_or_else(|| {
                    "collection cancellation outcome has no member identity".to_string()
                })?,
                &delivery.intent.delivery_id,
                context.control_epoch,
                delivery.status,
                matching_receipt,
            )?;
        }
        CollectionDeliveryRole::Join => {
            if join_is_superseded {
                record.supersede_join()?;
            } else {
                record.record_join_terminal(
                    &delivery.intent.delivery_id,
                    delivery.status == ReactionDeliveryStatus::Succeeded
                        || (delivery.status == ReactionDeliveryStatus::Skipped && matching_receipt),
                )?;
            }
        }
        CollectionDeliveryRole::MemberDescendant
        | CollectionDeliveryRole::CancellationDescendant
        | CollectionDeliveryRole::JoinDescendant => {
            return Err("collection descendant cannot own workflow aggregation".to_string());
        }
    }
    let continuation = continuation_intents(&mut record, workflow_sequence, &context.actions)?;
    super::commit_collection_delivery_outcome(
        store,
        expected_delivery_sequence,
        delivery,
        workflow_sequence,
        &record,
        &continuation,
    )
    .await
    .map_err(|error| error.to_string())?;
    metrics::record_terminal_commit(was_terminal, prior_member_status, context, &record);
    Ok(true)
}

/// Atomically reopen a failed join and its governed delivery journal.
pub(crate) async fn commit_manual_join_retry(
    store: &BoxedEventStore,
    expected_delivery_sequence: u64,
    delivery: &ReactionDeliveryRecord,
) -> Result<bool, String> {
    let Some(context) = delivery.intent.collection.as_ref() else {
        return Ok(false);
    };
    if context.role != CollectionDeliveryRole::Join {
        return Err("manual retry is forbidden for collection member lineages".to_string());
    }
    let (mut record, workflow_sequence) =
        load_collection_record(store, &delivery.intent.tenant, &context.workflow_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "collection workflow journal is missing".to_string())?;
    record.record_join_retry(&delivery.intent.delivery_id)?;
    let delivery_append =
        crate::trigger::delivery::delivery_record_append(expected_delivery_sequence, delivery)
            .map_err(|error| error.to_string())?;
    let workflow_append = super::workflow_append(
        &record,
        workflow_sequence,
        "CollectionWorkflow::JoinRetryV1",
    )
    .map_err(|error| error.to_string())?;
    store
        .append_batch(&[delivery_append, workflow_append])
        .await
        .map_err(|error| error.to_string())?;
    Ok(true)
}

fn continuation_intents(
    record: &mut super::CollectionWorkflowRecordV1,
    sequence: u64,
    owned_actions: &super::CollectionDeliveryActions,
) -> Result<Vec<crate::trigger::delivery::PersistedReactionIntent>, String> {
    let actions = owned_actions.borrowed();
    let mut continuation = match record.status {
        CollectionWorkflowStatus::Running => {
            super::admit_collection_window(record, sequence, &actions)?
        }
        CollectionWorkflowStatus::Cancelling | CollectionWorkflowStatus::TimingOut => {
            super::collection_cancellation_intents(record, sequence, &actions)?
        }
        _ => Vec::new(),
    };
    if let Some(join) = super::collection_join_intent(record, sequence, &actions)? {
        continuation.push(join);
    }
    Ok(continuation)
}
