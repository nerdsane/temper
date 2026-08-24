//! Classification, retry, and terminal metric helpers for durable deliveries.

pub(super) fn is_transient_delivery_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    [
        "timeout",
        "temporar",
        "mailbox",
        "deferred",
        "connection",
        "storage",
        "unavailable",
        "sequenceconflict",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

pub(super) fn is_expected_target_drop(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("not valid from state") || normalized.contains("blocked from state")
}

pub(super) fn automatic_retry_backoff(attempts: u32) -> chrono::Duration {
    match attempts {
        0 | 1 => chrono::Duration::milliseconds(100),
        2 => chrono::Duration::milliseconds(500),
        3 => chrono::Duration::seconds(2),
        _ => chrono::Duration::seconds(5),
    }
}

pub(super) fn record_delivery_terminal_metrics(
    record: &crate::trigger::delivery::ReactionDeliveryRecord,
) {
    use crate::trigger::delivery::ReactionDeliveryStatus;

    let outcome = match record.status {
        ReactionDeliveryStatus::Succeeded => "succeeded",
        ReactionDeliveryStatus::Skipped => "skipped",
        ReactionDeliveryStatus::DroppedAllowed => "dropped_allowed",
        ReactionDeliveryStatus::Rejected => "rejected",
        ReactionDeliveryStatus::DeadLettered => "dead_lettered",
        ReactionDeliveryStatus::Pending
        | ReactionDeliveryStatus::Claimed
        | ReactionDeliveryStatus::Dispatching => return,
    };
    let age = temper_runtime::scheduler::sim_now()
        .signed_duration_since(record.intent.created_at)
        .to_std()
        .unwrap_or_default();
    crate::runtime_metrics::record_reaction_delivery_outcome(
        record.intent.kind.metric_label(),
        outcome,
        record.attempts,
        age,
    );
}
