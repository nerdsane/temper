//! Exact target-receipt lookup for collection delivery aggregation.

use crate::storage::BoxedEventStore;
use crate::trigger::delivery::ReactionDeliveryRecord;

pub(super) async fn has_matching_target_receipt(
    store: &BoxedEventStore,
    delivery: &ReactionDeliveryRecord,
) -> Result<bool, String> {
    let Some(target_id) = delivery.intent.target_entity_id.as_deref() else {
        return Ok(false);
    };
    let rule: crate::trigger::types::ReactionRule =
        serde_json::from_value(delivery.intent.rule.clone()).map_err(|error| error.to_string())?;
    let persistence_id = match delivery.intent.schema_pin.as_ref() {
        Some(pin) => format!(
            "{}:{}:{}",
            delivery.intent.tenant,
            rule.then.entity_type,
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                target_id,
                &pin.execution,
            )
        ),
        None => format!(
            "{}:{}:{}",
            delivery.intent.tenant, rule.then.entity_type, target_id
        ),
    };
    let events = store
        .read_latest_events(
            &persistence_id,
            crate::entity_actor::types::MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(events.iter().any(|event| {
        crate::trigger::delivery::extract_receipt(&event.payload)
            .ok()
            .flatten()
            .is_some_and(|receipt| receipt.delivery_id == delivery.intent.delivery_id)
    }))
}
