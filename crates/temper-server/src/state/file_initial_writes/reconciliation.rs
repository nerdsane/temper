//! Cancellation-safe reconciliation after an atomic File commit.

use crate::entity_actor::EntityState;
use temper_runtime::tenant::TenantId;

use super::{FileStreamContentError, ServerState};

impl ServerState {
    pub(super) async fn reconcile_initial_file_commit(
        &self,
        tenant: &TenantId,
        file_id: &str,
        state: &EntityState,
    ) -> Result<(), FileStreamContentError> {
        let eviction_fence =
            self.fence_state_timeout_before_actor_eviction(tenant, "File", file_id, state);
        let drained = self
            .stop_and_remove_entity_incarnation(tenant, "File", file_id, None)
            .await;

        let mut first_error = None;
        if drained
            && self
                .ensure_entity_actor_materialized(tenant, "File", file_id)
                .await
        {
            // Hydration replaced the inactive fence with the authoritative
            // timed owner, or retained it as the untimed high-water mark.
        } else if drained {
            let synthetic_fence =
                self.reconcile_state_timeout_after_synthetic_commit(tenant, "File", file_id, state);
            self.release_inactive_state_timeout_after_actor_eviction(
                tenant,
                "File",
                file_id,
                synthetic_fence.or(eviction_fence),
            );
            first_error = Some(FileStreamContentError::State(format!(
                "File('{file_id}') committed initial content but failed to materialize its authoritative actor"
            )));
        } else {
            tracing::error!(
                tenant = %tenant,
                file_id,
                "File initial-content commit could not drain its stale actor; timeout fence retained"
            );
            first_error = Some(FileStreamContentError::State(format!(
                "File('{file_id}') committed initial content but failed to drain its stale actor"
            )));
        }

        if let Some(query_plane) = self.query_plane_store() {
            let fields = self.query_projection_fields(tenant, "File", &state.fields);
            let projected_state = self.query_projection_state(state);
            if let Err(error) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    "File",
                    file_id,
                    &state.status,
                    &fields,
                    &projected_state,
                    state.sequence_nr,
                )
                .await
            {
                tracing::error!(
                    tenant = %tenant,
                    file_id,
                    error = %error,
                    "query projection write failed after atomic File initial-content commit"
                );
                first_error.get_or_insert_with(|| {
                    FileStreamContentError::State(format!(
                        "query projection write failed during atomic File initial content create: {error}"
                    ))
                });
            }
        }

        match self.entity_index.write() {
            Ok(mut index) => {
                index
                    .entry(format!("{tenant}:File"))
                    .or_default()
                    .insert(file_id.to_string());
            }
            Err(poisoned) => {
                tracing::error!(
                    tenant = %tenant,
                    file_id,
                    "entity index lock poisoned after atomic File initial-content commit; recovering guarded state"
                );
                poisoned
                    .into_inner()
                    .entry(format!("{tenant}:File"))
                    .or_default()
                    .insert(file_id.to_string());
            }
        }
        crate::runtime_metrics::record_server_state_metrics(self);

        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }
}
