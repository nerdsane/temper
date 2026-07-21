//! Actor readiness, drain retry, and UID-safe registry eviction.

use tracing::instrument;

use temper_runtime::actor::ActorRef;
use temper_runtime::tenant::TenantId;

use crate::entity_actor::{EntityMsg, EntityResponse};
use crate::runtime_metrics;
use crate::state::ServerState;
use crate::state::dispatch::retry;

impl ServerState {
    pub(super) async fn get_or_spawn_tenant_actor_with_fields_when_ready(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor_with_fields_when_ready_guarded(
            tenant,
            entity_type,
            entity_id,
            initial_fields,
            false,
        )
        .await
    }

    async fn get_or_spawn_tenant_actor_with_fields_when_ready_guarded(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        require_entity_index: bool,
    ) -> Option<ActorRef<EntityMsg>> {
        const READINESS_RETRY_BUDGET: usize = 3;

        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
        let mut retried_absence = false;
        let mut readiness_retries = 0_usize;
        loop {
            if let Some(actor_ref) = self.get_or_spawn_tenant_actor_with_fields_guarded(
                tenant,
                entity_type,
                entity_id,
                initial_fields.clone(),
                require_entity_index,
            ) {
                let actor_uid = actor_ref.id().uid;
                self.state_timeout_tracker
                    .wait_for_hydration(tenant, entity_type, entity_id, actor_uid)
                    .await;
                let policy = self.dispatch_retry_policy();
                let readiness = retry::ask_with_backoff::<_, EntityResponse, _>(
                    &actor_ref,
                    || EntityMsg::GetState,
                    &policy,
                )
                .await;
                match readiness.result {
                    Ok(response) if response.state.status == "Deleted" => {
                        self.retire_deleted_hydration_if_current(
                            tenant,
                            entity_type,
                            entity_id,
                            actor_uid,
                            &response,
                        )
                        .await;
                        return None;
                    }
                    Ok(response) => {
                        let is_current = self.actor_registry.read().is_ok_and(|registry| {
                            registry.get(&actor_key).is_some_and(|current| {
                                !current.is_draining()
                                    && !current.is_stopped()
                                    && current.id().uid == actor_uid
                            })
                        });
                        if is_current {
                            self.reconcile_ready_actor_state_timeout(
                                tenant,
                                entity_type,
                                entity_id,
                                actor_uid,
                                &response,
                            );
                            return Some(actor_ref);
                        }
                    }
                    Err(
                        temper_runtime::actor::ActorError::Stopped
                        | temper_runtime::actor::ActorError::SendFailed,
                    ) => {
                        if actor_ref.is_drain_fenced() {
                            actor_ref.wait_for_drain_completion().await;
                        }
                    }
                    Err(_) => return None,
                }
                readiness_retries = readiness_retries.saturating_add(1);
                if readiness_retries >= READINESS_RETRY_BUDGET {
                    return None;
                }
                retried_absence = false;
                continue;
            }

            let draining = self
                .actor_registry
                .read()
                .ok()
                .and_then(|registry| registry.get(&actor_key).cloned())
                .filter(|actor_ref| actor_ref.is_draining());
            if let Some(draining) = draining {
                draining.wait_for_drain_completion().await;
                retried_absence = false;
                continue;
            }

            // The drain owner may have removed the registry entry between the
            // failed lookup and the inspection above. Retry that absence once;
            // a second miss means the table/spawn itself is unavailable.
            if retried_absence {
                return None;
            }
            retried_absence = true;
            tokio::task::yield_now().await;
        }
    }

    pub(crate) async fn get_or_spawn_tenant_actor_when_ready(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor_with_fields_when_ready(
            tenant,
            entity_type,
            entity_id,
            serde_json::json!({}),
        )
        .await
    }

    /// Materialize a memory-only actor only while its authoritative index entry exists.
    pub(super) async fn get_or_spawn_indexed_actor_when_ready(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor_with_fields_when_ready_guarded(
            tenant,
            entity_type,
            entity_id,
            serde_json::json!({}),
            true,
        )
        .await
    }

    pub(crate) async fn ask_actor_with_drain_retry<R, F>(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        mut actor_ref: ActorRef<EntityMsg>,
        make_msg: F,
        policy: &retry::RetryPolicy,
    ) -> (ActorRef<EntityMsg>, retry::AskResult<R>)
    where
        R: Send + 'static,
        F: Fn() -> EntityMsg,
    {
        const DRAIN_RETRY_BUDGET: usize = 3;

        let mut total_attempts = 0_u32;
        let mut total_elapsed = std::time::Duration::ZERO;
        let mut drain_retries = 0_usize;
        loop {
            let mut outcome =
                retry::ask_with_backoff::<_, R, _>(&actor_ref, &make_msg, policy).await;
            total_attempts = total_attempts.saturating_add(outcome.attempts);
            total_elapsed = total_elapsed.saturating_add(outcome.elapsed);
            let rejected_by_drain = matches!(
                outcome.result.as_ref(),
                Err(temper_runtime::actor::ActorError::Stopped
                    | temper_runtime::actor::ActorError::SendFailed)
            );
            let retry_after_lifecycle_change = rejected_by_drain
                && (actor_ref.is_drain_fenced() || !actor_ref.is_stopped())
                && drain_retries < DRAIN_RETRY_BUDGET;
            if retry_after_lifecycle_change {
                if actor_ref.is_drain_fenced() {
                    actor_ref.wait_for_drain_completion().await;
                }
                if let Some(replacement) = self
                    .get_or_spawn_tenant_actor_when_ready(tenant, entity_type, entity_id)
                    .await
                {
                    actor_ref = replacement;
                    drain_retries += 1;
                    continue;
                }
            }

            outcome.attempts = total_attempts;
            outcome.elapsed = total_elapsed;
            outcome.retried_after_transient |= drain_retries > 0;
            return (actor_ref, outcome);
        }
    }

    /// Remove an entity immediately using the legacy synchronous API.
    ///
    /// This compatibility path fences new mailbox traffic and removes only the
    /// incarnation observed by this call. New durability-sensitive code should
    /// use [`Self::drain_and_remove_entity`] so admitted work finishes first.
    #[instrument(skip_all, fields(otel.name = "entity.remove_entity", tenant = %tenant, entity_type, entity_id))]
    pub fn remove_entity(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
        let actor_ref = {
            let registry = match self.actor_registry.write() {
                Ok(registry) => registry,
                Err(poisoned) => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        "actor registry lock poisoned while claiming synchronous removal; recovering guarded state"
                    );
                    poisoned.into_inner()
                }
            };
            let actor_ref = registry.get(&actor_key).cloned();
            if actor_ref.is_none() {
                self.remove_entity_bookkeeping(tenant, entity_type, entity_id, true);
                debug_assert!(
                    !self.entity_exists(tenant, entity_type, entity_id),
                    "index-only removal must finish before releasing publication"
                );
                debug_assert!(
                    matches!(
                        self.actor_registry.try_write(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ),
                    "index-only removal must retain the actor-publication fence"
                );
            }
            actor_ref
        };
        let Some(actor_ref) = actor_ref else {
            runtime_metrics::record_server_state_metrics(self);
            return;
        };
        let actor_uid = actor_ref.id().uid;
        if let Err(error) = actor_ref.stop() {
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                actor_uid = %actor_uid,
                error = %error,
                "synchronous entity eviction left the live incarnation registered because its stop barrier was not admitted"
            );
            return;
        }

        if actor_ref.is_stopped() {
            let _ = self.remove_entity_actor_incarnation_if_current(
                tenant,
                entity_type,
                entity_id,
                Some(actor_uid),
                true,
            );
            return;
        }

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                actor_uid = %actor_uid,
                "synchronous entity eviction committed its stop barrier outside a Tokio runtime; guarded cleanup remains registry-visible"
            );
            return;
        };
        let state = self.clone();
        let tenant = tenant.clone();
        let entity_type = entity_type.to_string();
        let entity_id = entity_id.to_string();
        runtime.spawn(async move {
            // determinism-ok: one bounded cleanup task per accepted compatibility stop
            let _ = state
                .stop_and_remove_entity_if_current(&tenant, &entity_type, &entity_id, actor_uid)
                .await;
        });
    }

    /// Drain an entity actor, then remove it from the registry and index.
    #[instrument(skip_all, fields(otel.name = "entity.drain_and_remove_entity", tenant = %tenant, entity_type, entity_id))]
    pub async fn drain_and_remove_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) {
        let _ = self
            .stop_and_remove_entity_incarnation(tenant, entity_type, entity_id, None)
            .await;
    }

    /// Stop and evict only the actor incarnation that produced a captured result.
    ///
    /// Returns `false` without touching indexes when a newer incarnation owns
    /// the registry key.
    pub(crate) async fn stop_and_remove_entity_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        actor_uid: uuid::Uuid,
    ) -> bool {
        self.stop_and_remove_entity_incarnation(tenant, entity_type, entity_id, Some(actor_uid))
            .await
    }

    pub(crate) async fn stop_and_remove_entity_incarnation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        expected_actor_uid: Option<uuid::Uuid>,
    ) -> bool {
        const EVICTION_RETRY_BUDGET: usize = 3;

        for _ in 0..EVICTION_RETRY_BUDGET {
            let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
            let actor_ref = match self.actor_registry.read() {
                Ok(registry) => registry.get(&actor_key).cloned(),
                Err(poisoned) => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        "actor registry lock poisoned while beginning entity eviction; recovering guarded state"
                    );
                    poisoned.into_inner().get(&actor_key).cloned()
                }
            };
            let Some(actor_ref) = actor_ref else {
                if expected_actor_uid.is_some() {
                    return false;
                }
                if self.remove_entity_actor_incarnation_if_current(
                    tenant,
                    entity_type,
                    entity_id,
                    None,
                    true,
                ) {
                    return true;
                }
                continue;
            };
            if expected_actor_uid.is_some_and(|expected| expected != actor_ref.id().uid) {
                return false;
            }

            let actor_uid = actor_ref.id().uid;
            let drain_guard = match actor_ref.stop_and_wait().await {
                Ok(drain_guard) => drain_guard,
                Err(error) => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        actor_uid = %actor_uid,
                        error = %error,
                        "failed to drain actor before entity eviction"
                    );
                    return false;
                }
            };
            let removed = self.remove_entity_actor_incarnation_if_current(
                tenant,
                entity_type,
                entity_id,
                Some(actor_uid),
                true,
            );
            drop(drain_guard);
            if removed || expected_actor_uid.is_some() {
                return removed;
            }
        }

        false
    }

    /// Remove a stopped incarnation while its drain owner still fences actor
    /// publication. Registry ownership is retained through every associated
    /// in-memory index mutation, so a replacement cannot be erased by stale
    /// cleanup after it becomes visible.
    pub(crate) fn remove_entity_actor_incarnation_if_current(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        expected_actor_uid: Option<uuid::Uuid>,
        remove_from_entity_index: bool,
    ) -> bool {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");

        let mut registry = match self.actor_registry.write() {
            Ok(registry) => registry,
            Err(poisoned) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    "actor registry lock poisoned while detaching entity; recovering guarded state"
                );
                poisoned.into_inner()
            }
        };
        match expected_actor_uid {
            Some(expected_actor_uid)
                if registry
                    .get(&actor_key)
                    .is_none_or(|actor_ref| actor_ref.id().uid != expected_actor_uid) =>
            {
                return false;
            }
            None if registry.contains_key(&actor_key) => return false,
            _ => {}
        }
        registry.remove(&actor_key);

        self.remove_entity_bookkeeping(tenant, entity_type, entity_id, remove_from_entity_index);
        drop(registry);
        runtime_metrics::record_server_state_metrics(self);
        true
    }

    fn remove_entity_bookkeeping(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        remove_from_entity_index: bool,
    ) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
        match self.last_accessed.write() {
            Ok(mut last_accessed) => {
                last_accessed.remove(&actor_key);
            }
            Err(poisoned) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    "last-accessed lock poisoned while detaching entity; recovering guarded state"
                );
                poisoned.into_inner().remove(&actor_key);
            }
        }
        if remove_from_entity_index {
            let index_key = format!("{tenant}:{entity_type}");
            match self.entity_index.write() {
                Ok(mut index) => {
                    if let Some(ids) = index.get_mut(&index_key) {
                        ids.remove(entity_id);
                    }
                }
                Err(poisoned) => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        "entity-index lock poisoned while detaching entity; recovering guarded state"
                    );
                    if let Some(ids) = poisoned.into_inner().get_mut(&index_key) {
                        ids.remove(entity_id);
                    }
                }
            }
        }
    }
}
