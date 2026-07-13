//! Metrics for the hard per-entity event cap.

use std::sync::OnceLock;

use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue, global};

fn exhausted_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        global::meter("temper.runtime")
            .u64_counter("temper_entity_event_budget_exhausted_total")
            .with_description(
                "Actions refused because the hard per-entity event budget was exhausted.",
            )
            .build()
    })
}

pub fn record_exhausted(tenant: &str, entity_type: &str, entity_id: &str, workspace_id: &str) {
    let workspace_id = if workspace_id.is_empty() {
        "none"
    } else {
        workspace_id
    };
    exhausted_counter().add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("entity_id", entity_id.to_string()),
            KeyValue::new("workspace_id", workspace_id.to_string()),
        ],
    );
}

fn field_update_replay_skip_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        global::meter("temper.runtime")
            .u64_counter("temper_entity_field_update_replay_skip_total")
            .with_description(
                "FieldsUpdated/FieldsReplaced journal envelopes skipped during actor replay (schema parse failure).",
            )
            .build()
    })
}

/// Metric for ARN-189 replay path when a journaled field update cannot be deserialized.
pub fn record_field_update_replay_skip(tenant: &str, entity_type: &str, entity_id: &str) {
    field_update_replay_skip_counter().add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("entity_id", entity_id.to_string()),
        ],
    );
}
