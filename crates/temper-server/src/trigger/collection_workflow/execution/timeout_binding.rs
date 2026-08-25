//! Binding of one workflow to its exact ADR-0178 clock.

pub(super) fn bind_timeout_from_source(
    record: &mut super::super::CollectionWorkflowRecordV1,
    source_append: &temper_runtime::persistence::PersistenceAppend,
    timeout_action: &str,
) -> Result<(), String> {
    let event = source_append
        .events
        .first()
        .ok_or_else(|| "collection start source event is missing".to_string())?;
    let candidates = crate::trigger::delivery::extract_intents(&event.payload)?
        .into_iter()
        .filter(|intent| intent.kind == crate::trigger::delivery::DeliveryKind::StateTimeout)
        .filter_map(|intent| {
            let rule: crate::trigger::types::ReactionRule =
                serde_json::from_value(intent.rule.clone()).ok()?;
            (rule.then.action == timeout_action).then_some(intent)
        })
        .collect::<Vec<_>>();
    let [intent] = candidates.as_slice() else {
        return Err(
            "collection start requires exactly one matching ADR-0178 timeout intent".to_string(),
        );
    };
    if intent.source_entity_type != record.source_entity_type
        || intent.source_entity_id != record.source_entity_id
        || intent.source_sequence != record.source_sequence
        || intent.schema_pin != record.schema_pin
    {
        return Err(
            "collection timeout intent does not match committed source evidence".to_string(),
        );
    }
    let clock = intent
        .state_timeout
        .as_ref()
        .ok_or_else(|| "collection timeout intent lacks its ADR-0178 clock".to_string())?;
    record.bind_timeout(super::super::CollectionTimeoutBinding {
        delivery_id: intent.delivery_id.clone(),
        timeout_action: timeout_action.to_string(),
        state: clock.state.clone(),
        deadline: intent
            .not_before
            .ok_or_else(|| "collection timeout intent lacks its fixed deadline".to_string())?,
        declaration_id: clock.declaration_id.clone(),
        clock_sequence: clock.clock_sequence,
        schema_digest: clock.schema_digest.clone(),
    })?;
    Ok(())
}
