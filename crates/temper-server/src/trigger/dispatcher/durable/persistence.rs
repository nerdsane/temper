//! Typed terminal failure persistence and observation.

pub(super) async fn persist_terminal_delivery(
    state: &crate::ServerState,
    store: &crate::storage::BoxedEventStore,
    sequence: u64,
    record: &crate::trigger::delivery::ReactionDeliveryRecord,
) -> Result<(), String> {
    if !crate::trigger::collection_workflow::commit_terminal_delivery(store, sequence, record)
        .await?
    {
        crate::trigger::delivery::append_delivery_record(store, sequence, record)
            .await
            .map_err(|error| error.to_string())?;
    }
    if let Some(failure) = record.failure.as_ref() {
        let event_sequence = state.next_entity_event_sequence(
            &record.intent.tenant,
            &record.intent.source_entity_type,
            &record.intent.source_entity_id,
        );
        state.record_entity_observe_event_with_seq(
            &record.intent.tenant,
            &record.intent.source_entity_type,
            &record.intent.source_entity_id,
            event_sequence,
            "typed_delivery_failure",
            serde_json::json!({
                "seq": event_sequence,
                "delivery_id": record.intent.delivery_id,
                "delivery_kind": record.intent.kind,
                "trigger_name": record.intent.trigger_name,
                "failure": crate::failure_observation::redacted_failure_value(failure),
            }),
        );
    }
    Ok(())
}

pub(super) fn assign_typed_failure(
    record: &mut crate::trigger::delivery::ReactionDeliveryRecord,
    kind: crate::trigger::delivery::DurableFailureKind,
) -> Result<(), String> {
    assign_typed_failure_with_decision(record, kind, None)
}

pub(super) fn assign_typed_failure_with_decision(
    record: &mut crate::trigger::delivery::ReactionDeliveryRecord,
    kind: crate::trigger::delivery::DurableFailureKind,
    decision_id: Option<&str>,
) -> Result<(), String> {
    record.failure = Some(
        crate::trigger::delivery::delivery_failure_envelope(
            &record.intent,
            record.attempts,
            kind,
            record.last_error.as_deref(),
            decision_id,
        )
        .map_err(|error| format!("invalid durable failure adapter output: {error}"))?,
    );
    Ok(())
}
