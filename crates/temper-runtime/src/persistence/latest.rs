use super::{PersistenceEnvelope, PersistenceError};

/// Maximum number of persistence streams in one latest-event read.
///
/// Entity discovery can return a large tenant-wide candidate set. Callers must
/// consume it in bounded chunks so SQL parameter lists, Redis pipelines, and
/// decoded response buffers stay bounded.
pub const LATEST_EVENT_BATCH_SIZE: usize = 256;

/// Validate the shared latest-event batch budget.
pub fn validate_latest_event_batch(persistence_ids: &[String]) -> Result<(), PersistenceError> {
    if persistence_ids.len() > LATEST_EVENT_BATCH_SIZE {
        return Err(PersistenceError::Storage(format!(
            "latest-event batch contains {} streams; budget is {LATEST_EVENT_BATCH_SIZE}",
            persistence_ids.len()
        )));
    }
    Ok(())
}

/// Return whether an event is the canonical entity-deletion tombstone.
///
/// Older journals use the event type, generated direct deletes encode the
/// `Deleted` action, and composite sub-writes may retain their domain action
/// name while transitioning to the terminal `Deleted` status. All three forms
/// have identical lifecycle semantics.
pub fn is_deletion_tombstone(event: &PersistenceEnvelope) -> bool {
    event.event_type == "Deleted"
        || event
            .payload
            .get("action")
            .and_then(serde_json::Value::as_str)
            == Some("Deleted")
        || event
            .payload
            .get("to_status")
            .and_then(serde_json::Value::as_str)
            == Some("Deleted")
}
