//! External bound actions may recover existing entities, never create them.

use temper_runtime::{actor::ActorRef, tenant::TenantId};

use super::{DispatchError, ServerState, entity_ops::AuthzResourceSnapshot};
use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};

impl ServerState {
    /// Prune only this failed incarnation, never a concurrently installed actor.
    pub(crate) fn remove_failed_bound_actor(&self, actor: &ActorRef<EntityMsg>) {
        if !actor.is_closed() {
            return;
        }
        let key = &actor.id().name;
        let mut registry = self
            .actor_registry
            .write()
            .expect("actor registry lock poisoned");
        if registry
            .get(key)
            .is_some_and(|registered| registered.id() == actor.id())
        {
            registry.remove(key);
            self.last_accessed
                .write()
                .expect("actor access lock poisoned")
                .remove(key);
        }
    }

    /// Side-effect-free resource read before Cedar. Neither the catalog nor a
    /// default initial state is evidence of durable existence.
    pub(crate) async fn load_bound_action_snapshot(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<AuthzResourceSnapshot, String> {
        let table = self
            .transition_table_for_dispatch(tenant, entity_type)
            .map_err(|error| error.to_string())?;
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let actor = self
            .actor_registry
            .read()
            .map_err(|error| error.to_string())?
            .get(&key)
            .filter(|actor| !actor.is_closed())
            .cloned();
        let durable = if let Some((store, backend)) = self.event_journal() {
            Some(
                crate::entity_actor::admission::recover_existing_state(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &table,
                    &store,
                    backend,
                    self.blob_store_for_tenant(tenant).ok().as_ref(),
                )
                .await
                .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        let state = match (durable, actor) {
            // A journal-backed actor/index cannot override authoritative absence.
            (Some(None), _) => None,
            (_, Some(actor)) => {
                // Preserve the actor-state authorization/idempotency contract;
                // optimistic catch-up rechecks this precondition after a race.
                let response = actor
                    .ask::<EntityResponse>(EntityMsg::GetState, self.action_dispatch_timeout)
                    .await
                    .map_err(|error| error.to_string())?;
                (response.state.status != "Deleted").then_some(response.state)
            }
            (Some(state), None) => state,
            // Only a live, explicitly initialized actor attests to in-memory
            // or virtual existence. A catalog entry alone cannot create one.
            (None, None) => None,
        };
        let exists = state.is_some();
        let state = state.unwrap_or_else(|| {
            EntityActor::build_initial_state(entity_type, entity_id, &table, &serde_json::json!({}))
        });
        let resource_attrs = self
            .build_authz_resource_attrs(
                tenant,
                entity_type,
                entity_id,
                &state.status,
                &state.fields,
            )
            .await?;
        Ok(AuthzResourceSnapshot {
            exists,
            current_state: EntityResponse {
                success: true,
                state,
                error: None,
                custom_effects: vec![],
                scheduled_actions: vec![],
                spawn_requests: vec![],
                spec_governed: true,
            },
            resource_attrs,
        })
    }

    /// Pin the actor incarnation after authorization. A later eviction must not
    /// route this request back through the internal get-or-create entry point.
    pub(crate) async fn admit_bound_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<ActorRef<EntityMsg>, DispatchError> {
        let current = self
            .load_bound_action_snapshot(tenant, entity_type, entity_id)
            .await
            .map_err(DispatchError::Internal)?;
        if !current.exists {
            return Err(DispatchError::NotFound(format!(
                "{entity_type}:{entity_id}"
            )));
        }
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        if let Some(actor) = self
            .actor_registry
            .read()
            .map_err(|error| DispatchError::Internal(error.to_string()))?
            .get(&key)
            .filter(|actor| !actor.is_closed())
            .cloned()
        {
            self.touch_actor_access(&key);
            return Ok(actor);
        }
        if self.event_journal().is_none() {
            return Err(DispatchError::NotFound(format!(
                "{entity_type}:{entity_id}"
            )));
        }
        self.spawn_tenant_actor(tenant, entity_type, entity_id, serde_json::json!({}), true)
            .map_err(DispatchError::Internal)
    }
}
