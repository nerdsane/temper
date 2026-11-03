//! Reading an entity that must already exist, without ever starting one.

use temper_runtime::actor::ActorRef;
use temper_runtime::tenant::TenantId;

use super::ServerState;
use super::dispatch::retry;
use crate::entity_actor::{EntityMsg, EntityResponse};

impl ServerState {
    /// The current state of an entity that exists, or `None` when it does
    /// not exist or was deleted.
    ///
    /// Existence comes from what the server knows, not from whether an actor
    /// is live: a live actor is asked directly; an id the index lists (an
    /// entity whose idle actor was passivated, for example) has its actor
    /// started again, but only while the index still lists it; any other id
    /// is loaded only through [`Self::load_entity_from_store`], which reads
    /// the journal first. An answer that is the deletion tombstone, or that
    /// arrives after the index dropped an id that was started from it, stops
    /// that actor and reads as `None`, so a delete that finishes meanwhile
    /// leaves nothing behind.
    /// `Err` means existence is unknown (a store or actor failure).
    pub(crate) async fn get_existing_entity_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<EntityResponse>, String> {
        // `from_index`: started because the index lists the id, so the answer
        // counts only if the index still lists it afterwards.
        let (actor_ref, from_index) = match self.live_actor(tenant, entity_type, entity_id) {
            Some(actor_ref) => (actor_ref, false),
            None if self.entity_exists(tenant, entity_type, entity_id) => {
                match self.get_or_spawn_indexed_actor(tenant, entity_type, entity_id)? {
                    Some(actor_ref) => (actor_ref, true),
                    None => return Ok(None),
                }
            }
            None => {
                if !self
                    .load_entity_from_store(tenant, entity_type, entity_id)
                    .await?
                {
                    return Ok(None);
                }
                match self.live_actor(tenant, entity_type, entity_id) {
                    Some(actor_ref) => (actor_ref, false),
                    None => return Ok(None),
                }
            }
        };
        let policy = self.dispatch_retry_policy();
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::GetState,
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor query failed: {e}"))?;
        if response.state.status == "Deleted"
            || (from_index && !self.entity_exists(tenant, entity_type, entity_id))
        {
            let _ = actor_ref.stop();
            self.remove_entity(tenant, entity_type, entity_id);
            return Ok(None);
        }
        Ok(Some(response))
    }

    fn live_actor(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let registry = self
            .actor_registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        registry
            .get(&key)
            .filter(|actor| !actor.is_closed())
            .cloned()
    }
}

#[cfg(test)]
#[path = "existing_entity_test.rs"]
mod tests;
