//! Durable post-dispatch classification and persistence.

use super::*;
use crate::trigger::delivery::{ReactionDeliveryStatus, append_delivery_record};

#[expect(
    clippy::too_many_arguments,
    reason = "settlement consumes the complete fenced dispatch outcome"
)]
pub(super) async fn settle_dispatch(
    state: &crate::ServerState,
    store: &crate::storage::BoxedEventStore,
    mut record: crate::trigger::delivery::ReactionDeliveryRecord,
    sequence: u64,
    intent: &crate::trigger::delivery::PersistedReactionIntent,
    awaited_collection_member: bool,
    automatic_attempt_budget: u32,
    drop_ok: bool,
    results: Vec<ReactionResult>,
) -> Result<Vec<ReactionResult>, String> {
    record.lease_expires_at = None;
    record.next_attempt_at = None;
    let awaited_callback_accepted = record.awaited_execution.as_ref().is_some_and(|evidence| {
        evidence.phase == crate::trigger::delivery::AwaitedExecutionPhase::CallbackAccepted
    });
    let awaited_callback_pending = record
        .awaited_execution
        .as_ref()
        .is_some_and(|evidence| evidence.callback_action.is_some() && !awaited_callback_accepted);
    let awaited_callback_failure = record
        .awaited_execution
        .as_ref()
        .and_then(|evidence| evidence.callback_failure);
    let awaited_failure = record
        .awaited_execution
        .as_ref()
        .and_then(|evidence| evidence.execution_failure);
    if awaited_collection_member && awaited_callback_pending {
        let error = results
            .iter()
            .find_map(|result| result.error.clone())
            .unwrap_or_else(|| {
                "AwaitedExecutionIncomplete: exact callback acceptance evidence is absent"
                    .to_string()
            });
        let transient = matches!(
            awaited_callback_failure,
            Some(
                crate::trigger::delivery::AwaitedExecutionFailureClass::CallbackTimeout
                    | crate::trigger::delivery::AwaitedExecutionFailureClass::CallbackStorageFailure
            )
        );
        record.transient_failure = transient;
        record.last_error = Some(error);
        record.status = if awaited_callback_failure.is_none() {
            ReactionDeliveryStatus::Rejected
        } else if transient && record.attempts < automatic_attempt_budget {
            record.next_attempt_at = Some(
                temper_runtime::scheduler::sim_now() + automatic_retry_backoff(record.attempts),
            );
            ReactionDeliveryStatus::Pending
        } else if transient {
            ReactionDeliveryStatus::DeadLettered
        } else {
            ReactionDeliveryStatus::Rejected
        };
    } else if awaited_collection_member && awaited_failure.is_some() {
        record.status = ReactionDeliveryStatus::Rejected;
        record.last_error = awaited_failure.map(|failure| format!("{failure:?}"));
    } else if awaited_collection_member && !awaited_callback_accepted {
        record.status = ReactionDeliveryStatus::Rejected;
        record.last_error = Some(
            "AwaitedExecutionIncomplete: exact callback acceptance evidence is absent".to_string(),
        );
    } else if results.iter().any(|result| result.success) {
        record.status = ReactionDeliveryStatus::Succeeded;
        record.last_error = None;
    } else if results.is_empty() {
        record.status = ReactionDeliveryStatus::Skipped;
        record.last_error = None;
    } else {
        let failed = results
            .iter()
            .find(|result| !result.success)
            .expect("unsuccessful result set contains a failure");
        let failure = failed
            .failure
            .unwrap_or(crate::trigger::types::ReactionFailureKind::LegacyDispatchFailure);
        let error = failed
            .error
            .clone()
            .unwrap_or_else(|| "reaction target rejected the action".to_string());
        let migrated_timeout =
            intent.state_timeout.is_some() && error.contains("migrated scoped schema write fence");
        let collection_control_skip = collection_control_skip_reason(
            intent.collection.as_ref().map(|context| context.role),
            &error,
        );
        let transient = is_transient_delivery_failure(failure);
        let dropped_allowed = drop_ok && is_expected_target_drop(&error);
        record.transient_failure = transient;
        record.last_error = Some(error);
        record.status = if migrated_timeout || collection_control_skip.is_some() {
            ReactionDeliveryStatus::Skipped
        } else if transient && record.attempts < automatic_attempt_budget {
            crate::runtime_metrics::record_reaction_delivery_event(
                intent.kind.metric_label(),
                "automatic_retry_scheduled",
            );
            record.next_attempt_at = Some(
                temper_runtime::scheduler::sim_now() + automatic_retry_backoff(record.attempts),
            );
            ReactionDeliveryStatus::Pending
        } else if transient {
            ReactionDeliveryStatus::DeadLettered
        } else if dropped_allowed {
            ReactionDeliveryStatus::DroppedAllowed
        } else {
            ReactionDeliveryStatus::Rejected
        };
        if let Some(reason) = collection_control_skip {
            record.last_error = Some(reason.to_string());
        }
        if record.status.is_terminal()
            && !matches!(
                record.status,
                ReactionDeliveryStatus::Skipped | ReactionDeliveryStatus::DroppedAllowed
            )
        {
            assign_typed_failure_with_decision(
                &mut record,
                crate::trigger::delivery::DurableFailureKind::Reaction(failure),
                failed.decision_id.as_deref(),
            )?;
        }
    }
    if record.status.is_terminal() {
        persist_terminal_delivery(state, store, sequence, &record).await?;
    } else {
        append_delivery_record(store, sequence, &record)
            .await
            .map_err(|error| error.to_string())?;
    }
    record_delivery_terminal_metrics(&record);
    Ok(results)
}
