//! Atomic terminal delivery aggregation.

use super::*;

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
        && (matches!(
            member.status,
            CollectionMemberStatus::Cancelled | CollectionMemberStatus::TimedOut
        ) || (record.requested_outcome.is_some()
            && member.status == CollectionMemberStatus::InFlight
            && member.receipt.is_some()))
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
    ) && super::super::active_source_workflow_id(store, &record)
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
                let receipt = super::super::CollectionMemberReceipt {
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
                record.record_member_terminal(super::super::CollectionMemberTerminalEvidence {
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
                record.record_member_terminal(super::super::CollectionMemberTerminalEvidence {
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
    super::super::commit_collection_delivery_outcome(
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
