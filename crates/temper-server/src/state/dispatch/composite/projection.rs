use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use crate::state::{ProjectionEnqueueOutcome, ServerState};
use crate::storage::QueryProjectionUpsert;

use super::{AtomicCompositeStream, DispatchError};

impl ServerState {
    pub(super) async fn update_composite_query_projections(
        &self,
        tenant: &TenantId,
        streams: &BTreeMap<String, AtomicCompositeStream>,
    ) -> Result<(), DispatchError> {
        let query_plane = self.query_plane_store();
        let projection_queue = self
            .query_projection_queue
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        let mut projection_upserts = Vec::new();
        let mut projection_removals = Vec::new();

        for stream in streams.values().filter(|stream| !stream.events.is_empty()) {
            self.cache_entity_status(
                format!("{tenant}:{}:{}", stream.entity_type, stream.entity_id),
                stream.state.status.clone(),
            );
            self.index_composite_stream(tenant, stream)?;
            self.clear_commons_storage_projection_cache_for_entity(&stream.entity_type);

            if query_plane.is_some() {
                self.queue_or_stage_projection(
                    tenant,
                    stream,
                    projection_queue.as_ref(),
                    &mut projection_upserts,
                    &mut projection_removals,
                );
            }
        }

        if let Some(query_plane) = query_plane {
            for (entity_type, entity_id, sequence_nr) in projection_removals {
                query_plane
                    .remove_projection_through_sequence(
                        tenant.as_str(),
                        &entity_type,
                        &entity_id,
                        sequence_nr,
                    )
                    .await
                    .map_err(|e| {
                        DispatchError::Internal(format!(
                            "query projection removal failed after composite batch: {e}"
                        ))
                    })?;
            }
            if !projection_upserts.is_empty() {
                query_plane
                    .upsert_projections(tenant.as_str(), &projection_upserts)
                    .await
                    .map_err(|e| {
                        DispatchError::Internal(format!(
                            "query projection write failed after composite batch: {e}"
                        ))
                    })?;
            }
        }

        Ok(())
    }

    fn index_composite_stream(
        &self,
        tenant: &TenantId,
        stream: &AtomicCompositeStream,
    ) -> Result<(), DispatchError> {
        let index_key = format!("{tenant}:{}", stream.entity_type);
        self.mutate_entity_index(tenant, &stream.entity_type, |index| {
            if stream.state.status == "Deleted" {
                if let Some(ids) = index.get_mut(&index_key) {
                    ids.remove(&stream.entity_id);
                }
            } else {
                index
                    .entry(index_key)
                    .or_default()
                    .insert(stream.entity_id.clone());
            }
        })
        .map_err(DispatchError::Internal)?;
        Ok(())
    }

    fn queue_or_stage_projection(
        &self,
        tenant: &TenantId,
        stream: &AtomicCompositeStream,
        projection_queue: Option<&std::sync::Arc<crate::state::QueryProjectionWriteQueue>>,
        projection_upserts: &mut Vec<QueryProjectionUpsert>,
        projection_removals: &mut Vec<(String, String, u64)>,
    ) {
        if stream.state.status == "Deleted" {
            if let Some(queue) = projection_queue {
                let outcome = queue.enqueue_remove(
                    tenant.to_string(),
                    stream.entity_type.clone(),
                    stream.entity_id.clone(),
                    stream.state.sequence_nr,
                    "queued_composite",
                );
                record_composite_projection_enqueue(tenant, stream, outcome);
            } else {
                projection_removals.push((
                    stream.entity_type.clone(),
                    stream.entity_id.clone(),
                    stream.state.sequence_nr,
                ));
            }
            return;
        }
        let fields = stream.state.fields.clone();
        let projected_state = self.query_projection_state(&stream.state);
        if let Some(queue) = projection_queue {
            let outcome = queue.enqueue_upsert(
                tenant.to_string(),
                stream.entity_type.clone(),
                stream.entity_id.clone(),
                stream.state.status.clone(),
                fields,
                projected_state,
                stream.state.sequence_nr,
                "queued_composite",
            );
            record_composite_projection_enqueue(tenant, stream, outcome);
        } else {
            let indexed_fields = self.query_projection_fields(tenant, &stream.entity_type, &fields);
            projection_upserts.push(QueryProjectionUpsert {
                entity_type: stream.entity_type.clone(),
                entity_id: stream.entity_id.clone(),
                status: stream.state.status.clone(),
                fields,
                state: projected_state,
                indexed_fields,
                sequence_nr: stream.state.sequence_nr,
                known_new: !stream.target_existed,
            });
        }
    }
}

fn record_composite_projection_enqueue(
    tenant: &TenantId,
    stream: &AtomicCompositeStream,
    outcome: ProjectionEnqueueOutcome,
) {
    match outcome {
        ProjectionEnqueueOutcome::Enqueued | ProjectionEnqueueOutcome::Coalesced => {
            crate::query_projection_metrics::record_update_enqueued(
                tenant.as_str(),
                &stream.entity_type,
                if stream.state.status == "Deleted" {
                    "remove"
                } else {
                    "upsert"
                },
                "queued_composite",
            );
        }
        ProjectionEnqueueOutcome::StaleSkipped => {}
        ProjectionEnqueueOutcome::Full => {
            tracing::error!(
                tenant = %tenant,
                entity_type = %stream.entity_type,
                entity_id = %stream.entity_id,
                sequence_nr = stream.state.sequence_nr,
                "bounded query projection queue full after composite batch"
            );
        }
    }
}
