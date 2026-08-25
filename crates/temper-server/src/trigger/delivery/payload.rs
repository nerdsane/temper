//! Durable intent and receipt payload encoding.

use super::{
    PersistedReactionIntent, REACTION_INTENTS_FIELD, REACTION_RECEIPT_FIELD, ReactionReceipt,
};

/// Attach normalized intents to the source event payload before its single append.
pub fn attach_intents(
    payload: &mut serde_json::Value,
    intents: &[PersistedReactionIntent],
) -> Result<(), String> {
    if intents.is_empty() {
        return Ok(());
    }
    let object = payload
        .as_object_mut()
        .ok_or_else(|| "entity event payload must be an object".to_string())?;
    object.insert(
        REACTION_INTENTS_FIELD.to_string(),
        serde_json::to_value(intents).map_err(|error| error.to_string())?,
    );
    Ok(())
}

/// Read normalized intents from a replayed source event payload.
pub fn extract_intents(
    payload: &serde_json::Value,
) -> Result<Vec<PersistedReactionIntent>, String> {
    let Some(value) = payload.get(REACTION_INTENTS_FIELD) else {
        return Ok(Vec::new());
    };
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
}

/// Attach one delivery receipt to the target event before its append.
pub fn attach_receipt(
    payload: &mut serde_json::Value,
    receipt: &ReactionReceipt,
) -> Result<(), String> {
    let object = payload
        .as_object_mut()
        .ok_or_else(|| "entity event payload must be an object".to_string())?;
    object.insert(
        REACTION_RECEIPT_FIELD.to_string(),
        serde_json::to_value(receipt).map_err(|error| error.to_string())?,
    );
    Ok(())
}

/// Read a co-committed target receipt from a replayed event payload.
pub fn extract_receipt(payload: &serde_json::Value) -> Result<Option<ReactionReceipt>, String> {
    let Some(value) = payload.get(REACTION_RECEIPT_FIELD) else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| error.to_string())
}
