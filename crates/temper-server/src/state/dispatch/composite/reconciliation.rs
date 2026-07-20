//! Cancellation-safe reconciliation after an atomic composite commit.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

use super::{AtomicCompositeStream, DispatchError};

pub(super) struct CompositeReconciliationTiming {
    pub(super) reload_ms: Option<u64>,
    pub(super) projection_ms: Option<u64>,
}

impl ServerState {
    pub(super) async fn reconcile_committed_composite_streams(
        &self,
        tenant: &TenantId,
        streams: BTreeMap<String, AtomicCompositeStream>,
        timing_enabled: bool,
    ) -> Result<CompositeReconciliationTiming, DispatchError> {
        let mut inactive_timeout_fences = BTreeMap::new();
        for (persistence_id, stream) in committed_streams(&streams) {
            if let Some(fence) = self.fence_state_timeout_before_actor_eviction(
                tenant,
                &stream.entity_type,
                &stream.entity_id,
                &stream.state,
            ) {
                inactive_timeout_fences.insert(persistence_id.clone(), fence);
            }
        }

        let mut drained_actor_ids = BTreeSet::new();
        let mut drained_actors = Vec::new();
        for (persistence_id, stream) in committed_streams(&streams) {
            // Keep the pre-commit incarnation registry-visible until its FIFO
            // mailbox is drained. Removing it first would let a replacement
            // hydrate the committed batch while the stale actor can still win
            // OCC and append a newer tail behind that replacement.
            let actor_key = format!("{tenant}:{}:{}", stream.entity_type, stream.entity_id);
            let actor_ref = match self.actor_registry.read() {
                Ok(registry) => registry.get(&actor_key).cloned(),
                Err(poisoned) => {
                    tracing::error!(
                        tenant = %tenant,
                        persistence_id,
                        "actor registry lock poisoned after composite commit; recovering guarded state"
                    );
                    poisoned.into_inner().get(&actor_key).cloned()
                }
            }
            .filter(|actor_ref| !actor_ref.is_stopped());
            if let Some(actor_ref) = actor_ref {
                drained_actor_ids.insert(persistence_id.clone());
                drained_actors.push((
                    persistence_id.clone(),
                    stream.entity_type.clone(),
                    stream.entity_id.clone(),
                    actor_ref.id().uid,
                    actor_ref,
                ));
            }
        }

        let mut first_error = None;
        for (persistence_id, entity_type, entity_id, actor_uid, actor_ref) in drained_actors {
            match actor_ref.stop_and_wait().await {
                Ok(drain_guard) => {
                    // The drain guard keeps replacement publication fenced
                    // until this UID-owned registry entry is gone.
                    let _ = self.remove_entity_actor_incarnation_if_current(
                        tenant,
                        &entity_type,
                        &entity_id,
                        Some(actor_uid),
                        true,
                    );
                    drop(drain_guard);
                }
                Err(error) => {
                    let message = format!(
                        "composite batch committed {persistence_id} but failed to drain its stale actor: {error}"
                    );
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        actor_uid = %actor_uid,
                        error = %error,
                        "post-commit composite actor drain failed"
                    );
                    first_error.get_or_insert_with(|| DispatchError::Internal(message));
                }
            }
        }

        let reload_started_at = timing_enabled.then(std::time::Instant::now);
        // No active synthetic timer is installed until all captured stale UIDs
        // have drained. A short-lived timer therefore cannot fire into an older
        // actor while another stream's drain is still blocked.
        for (persistence_id, stream) in committed_streams(&streams) {
            let eviction_fence = inactive_timeout_fences.remove(persistence_id);
            if stream.state.status == "Deleted" {
                self.release_inactive_state_timeout_after_actor_eviction(
                    tenant,
                    &stream.entity_type,
                    &stream.entity_id,
                    eviction_fence,
                );
                continue;
            }

            if !stream.target_existed && !drained_actor_ids.contains(persistence_id) {
                reconcile_synthetic_timeout(self, tenant, stream, eviction_fence);
                continue;
            }
            if self
                .ensure_entity_actor_materialized(tenant, &stream.entity_type, &stream.entity_id)
                .await
            {
                continue;
            }

            // The stale incarnation has already been fenced and drained. Even
            // if actor materialization is temporarily unavailable, the durable
            // clock still needs an owner so a successful commit cannot silently
            // lose its timeout.
            reconcile_synthetic_timeout(self, tenant, stream, eviction_fence);
            let message = format!(
                "composite batch committed {}:{} but failed to reload it",
                stream.entity_type, stream.entity_id
            );
            tracing::error!(
                tenant = %tenant,
                entity_type = %stream.entity_type,
                entity_id = %stream.entity_id,
                "post-commit composite actor materialization failed; synthetic timeout ownership retained"
            );
            first_error.get_or_insert_with(|| DispatchError::Internal(message));
        }
        let reload_ms = reload_started_at.map(|started| started.elapsed().as_millis() as u64);

        let projection_started_at = timing_enabled.then(std::time::Instant::now);
        if let Err(error) = self
            .update_composite_query_projections(tenant, &streams)
            .await
        {
            tracing::error!(
                tenant = %tenant,
                error = %error,
                "post-commit composite query projection reconciliation failed"
            );
            first_error.get_or_insert(error);
        }
        let projection_ms =
            projection_started_at.map(|started| started.elapsed().as_millis() as u64);

        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(CompositeReconciliationTiming {
            reload_ms,
            projection_ms,
        })
    }
}

fn committed_streams(
    streams: &BTreeMap<String, AtomicCompositeStream>,
) -> impl Iterator<Item = (&String, &AtomicCompositeStream)> {
    streams
        .iter()
        .filter(|(_, stream)| !stream.events.is_empty())
}

fn reconcile_synthetic_timeout(
    state: &ServerState,
    tenant: &TenantId,
    stream: &AtomicCompositeStream,
    eviction_fence: Option<super::super::state_timeouts::InactiveStateTimeoutFence>,
) {
    let synthetic_fence = state.reconcile_state_timeout_after_synthetic_commit(
        tenant,
        &stream.entity_type,
        &stream.entity_id,
        &stream.state,
    );
    state.release_inactive_state_timeout_after_actor_eviction(
        tenant,
        &stream.entity_type,
        &stream.entity_id,
        synthetic_fence.or(eviction_fence),
    );
}
