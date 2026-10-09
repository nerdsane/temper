//! Read-only recovery for external bound-action admission.

use temper_jit::table::TransitionTable;
use temper_runtime::actor::ActorError;

use super::EntityState;
use super::actor::{EntityActor, ReplayPolicy};
use crate::storage::{BackendLabel, BoxedEventStore};

/// Recover a validated snapshot/tail without appending an initialization event.
pub(crate) async fn recover_existing_state(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> Result<Option<EntityState>, ActorError> {
    let mut state =
        EntityActor::build_initial_state(entity_type, entity_id, table, &serde_json::json!({}));
    EntityActor::replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        ReplayPolicy::ExistingSnapshot,
    )
    .await?;
    if state.sequence_nr == 0 || state.status == "Deleted" {
        return Ok(None);
    }
    Ok(Some(state))
}
