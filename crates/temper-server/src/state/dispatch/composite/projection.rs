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
            for (entity_type, entity_id, sequence_nr) in projection_removals {
                query_plane
                    .remove_projection(tenant.as_str(), &entity_type, &entity_id, sequence_nr)
                    .await
                    .map_err(|e| {
                        DispatchError::Internal(format!(
                            "query projection removal failed after composite batch: {e}"
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
        let mut index = self.entity_index.write().map_err(|_| {
            DispatchError::Internal("entity index lock poisoned after composite batch".to_string())
        })?;
        let entity_ids = index.entry(index_key).or_default();
        if stream.state.status == "Deleted" {
            entity_ids.remove(&stream.entity_id);
        } else {
            entity_ids.insert(stream.entity_id.clone());
        }
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
        if let Some(queue) = projection_queue {
            let outcome = if stream.state.status == "Deleted" {
                queue.enqueue_remove(
                    tenant.to_string(),
                    stream.entity_type.clone(),
                    stream.entity_id.clone(),
                    stream.state.sequence_nr,
                    "queued_composite",
                )
            } else {
                queue.enqueue_upsert(
                    tenant.to_string(),
                    stream.entity_type.clone(),
                    stream.entity_id.clone(),
                    stream.state.status.clone(),
                    stream.state.fields.clone(),
                    self.query_projection_state(&stream.state),
                    stream.state.sequence_nr,
                    "queued_composite",
                )
            };
            record_composite_projection_enqueue(tenant, stream, outcome);
        } else if stream.state.status == "Deleted" {
            projection_removals.push((
                stream.entity_type.clone(),
                stream.entity_id.clone(),
                stream.state.sequence_nr,
            ));
        } else {
            let fields = stream.state.fields.clone();
            let indexed_fields = self.query_projection_fields(tenant, &stream.entity_type, &fields);
            projection_upserts.push(QueryProjectionUpsert {
                entity_type: stream.entity_type.clone(),
                entity_id: stream.entity_id.clone(),
                status: stream.state.status.clone(),
                fields,
                state: self.query_projection_state(&stream.state),
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
    let operation = if stream.state.status == "Deleted" {
        "remove"
    } else {
        "upsert"
    };
    match outcome {
        ProjectionEnqueueOutcome::Enqueued | ProjectionEnqueueOutcome::Coalesced => {
            crate::query_projection_metrics::record_update_enqueued(
                tenant.as_str(),
                &stream.entity_type,
                operation,
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
                operation,
                "bounded query projection queue full after composite batch"
            );
        }
    }
}
