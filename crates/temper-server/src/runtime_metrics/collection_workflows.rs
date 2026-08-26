//! Low-cardinality collection workflow metrics (ADR-0181).

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use super::metrics;
use crate::trigger::collection_workflow::{
    CollectionDeliveryRole, CollectionMemberStatus, CollectionWorkflowStatus,
};
use crate::trigger::delivery::ReactionDeliveryStatus;

pub(super) struct CollectionWorkflowMetrics {
    events_total: Counter<u64>,
    active_window: Histogram<u64>,
    queue_age_ms: Histogram<f64>,
    join_latency_ms: Histogram<f64>,
}

impl CollectionWorkflowMetrics {
    pub(super) fn new(meter: &Meter) -> Self {
        Self {
            events_total: meter
                .u64_counter("temper_collection_workflow_events_total")
                .with_description(
                    "ADR-0181: bounded collection lifecycle events with closed event and outcome labels.",
                )
                .build(),
            active_window: meter
                .u64_histogram("temper_collection_workflow_active_window")
                .with_description(
                    "ADR-0181: admitted non-terminal member count after a durable workflow commit.",
                )
                .build(),
            queue_age_ms: meter
                .f64_histogram("temper_collection_workflow_queue_age_ms")
                .with_unit("ms")
                .with_description(
                    "ADR-0181: collection member or cancellation delivery age at terminal outcome.",
                )
                .build(),
            join_latency_ms: meter
                .f64_histogram("temper_collection_workflow_join_latency_ms")
                .with_unit("ms")
                .with_description("ADR-0181: terminal collection join delivery latency.")
                .build(),
        }
    }
}

/// Record one closed collection lifecycle event without workflow or entity IDs.
pub(crate) fn record_collection_workflow_event(event: &'static str, outcome: &'static str) {
    metrics().collection_workflows.events_total.add(
        1,
        &[
            KeyValue::new("event", event),
            KeyValue::new("outcome", outcome),
        ],
    );
}

/// Record the current bounded member concurrency window.
pub(crate) fn record_collection_active_window(in_flight: u16) {
    metrics()
        .collection_workflows
        .active_window
        .record(u64::from(in_flight), &[]);
}

/// Record one exact durable member outcome using the closed ADR-0181 taxonomy.
pub(crate) fn record_collection_member_outcome(status: CollectionMemberStatus) {
    let outcome = match status {
        CollectionMemberStatus::Pending => "pending",
        CollectionMemberStatus::InFlight => "in_flight",
        CollectionMemberStatus::Succeeded => "succeeded",
        CollectionMemberStatus::Failed => "failed",
        CollectionMemberStatus::Cancelled => "cancelled",
        CollectionMemberStatus::TimedOut => "timed_out",
    };
    record_collection_workflow_event("member_outcome", outcome);
}

/// Record one immutable terminal workflow classification.
pub(crate) fn record_collection_terminal_classification(status: CollectionWorkflowStatus) {
    let classification = match status {
        CollectionWorkflowStatus::Succeeded => "succeeded",
        CollectionWorkflowStatus::PartiallyFailed => "partially_failed",
        CollectionWorkflowStatus::Failed => "failed",
        CollectionWorkflowStatus::Cancelled => "cancelled",
        CollectionWorkflowStatus::TimedOut => "timed_out",
        CollectionWorkflowStatus::Running => "running",
        CollectionWorkflowStatus::Cancelling => "cancelling",
        CollectionWorkflowStatus::TimingOut => "timing_out",
    };
    record_collection_workflow_event("terminal_classification", classification);
}

/// Record one terminal collection delivery using only closed labels.
pub(crate) fn record_collection_delivery_terminal(
    role: CollectionDeliveryRole,
    outcome: ReactionDeliveryStatus,
    attempts: u32,
    age: Duration,
) {
    let role = collection_delivery_role_label(role);
    let outcome = outcome_label(outcome);
    record_collection_workflow_event(role, outcome);
    if attempts > 1 {
        metrics().collection_workflows.events_total.add(
            u64::from(attempts - 1),
            &[
                KeyValue::new("event", "retry"),
                KeyValue::new("outcome", role),
            ],
        );
    }
    let age_ms = age.as_secs_f64() * 1000.0;
    if role == "join" {
        metrics()
            .collection_workflows
            .join_latency_ms
            .record(age_ms, &[KeyValue::new("outcome", outcome)]);
    } else {
        metrics().collection_workflows.queue_age_ms.record(
            age_ms,
            &[
                KeyValue::new("role", role),
                KeyValue::new("outcome", outcome),
            ],
        );
    }
}

/// Return the closed low-cardinality label for a collection delivery role.
pub(crate) fn collection_delivery_role_label(role: CollectionDeliveryRole) -> &'static str {
    match role {
        CollectionDeliveryRole::Member => "member",
        CollectionDeliveryRole::Cancellation => "cancellation",
        CollectionDeliveryRole::Join => "join",
        CollectionDeliveryRole::MemberDescendant => "member_descendant",
        CollectionDeliveryRole::CancellationDescendant => "cancellation_descendant",
        CollectionDeliveryRole::JoinDescendant => "join_descendant",
    }
}

fn outcome_label(outcome: ReactionDeliveryStatus) -> &'static str {
    match outcome {
        ReactionDeliveryStatus::Pending => "pending",
        ReactionDeliveryStatus::Claimed => "claimed",
        ReactionDeliveryStatus::Dispatching => "dispatching",
        ReactionDeliveryStatus::Succeeded => "succeeded",
        ReactionDeliveryStatus::Skipped => "skipped",
        ReactionDeliveryStatus::DroppedAllowed => "dropped_allowed",
        ReactionDeliveryStatus::Rejected => "rejected",
        ReactionDeliveryStatus::DeadLettered => "dead_lettered",
    }
}
