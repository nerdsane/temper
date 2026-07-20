//! Derived query-projection maintenance after dispatch.

use super::*;

impl crate::state::ServerState {
    pub(super) async fn apply_query_projection_update(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        debug_assert_eq!(
            query_projection_update_mode(),
            QueryProjectionUpdateMode::Queued
        );
        let Some(query_plane) = self.query_plane_store() else {
            return;
        };

        let tenant = ctx.tenant.to_string();
        let entity_type = ctx.entity_type.to_string();
        let entity_id = ctx.entity_id.to_string();
        let status = response.state.status.clone();
        let fields =
            self.query_projection_fields(ctx.tenant, ctx.entity_type, &response.state.fields);
        let projected_state = self.query_projection_state(&response.state);
        let sequence_nr = response.state.sequence_nr;
        let operation = if status == "Deleted" {
            "remove"
        } else {
            "upsert"
        }
        .to_string();
        let source = "queued_dispatch".to_string();
        let enqueued_at = Instant::now(); // determinism-ok: production-only projection queue metric
        let queue = self
            .query_projection_queue
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        if let Some(queue) = queue {
            let outcome = if status == "Deleted" {
                queue.enqueue_remove(
                    tenant.clone(),
                    entity_type.clone(),
                    entity_id.clone(),
                    sequence_nr,
                    "queued_dispatch",
                )
            } else {
                queue.enqueue_upsert(
                    tenant.clone(),
                    entity_type.clone(),
                    entity_id.clone(),
                    status.clone(),
                    fields,
                    projected_state,
                    sequence_nr,
                    "queued_dispatch",
                )
            };
            match outcome {
                ProjectionEnqueueOutcome::Enqueued | ProjectionEnqueueOutcome::Coalesced => {
                    crate::query_projection_metrics::record_update_enqueued(
                        &tenant,
                        &entity_type,
                        &operation,
                        &source,
                    );
                }
                ProjectionEnqueueOutcome::StaleSkipped => {
                    tracing::debug!(
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        sequence_nr,
                        operation = %operation,
                        "skipped stale queued query projection update"
                    );
                }
                ProjectionEnqueueOutcome::Full => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        sequence_nr,
                        operation = %operation,
                        "bounded query projection queue full; acknowledged dispatch will rely on repair/backfill"
                    );
                }
            }
            return;
        }

        let span = tracing::info_span!(
            "dispatch.phase.query_projection",
            otel.name = "dispatch.phase.query_projection",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            operation = %operation,
        );

        // Boxed so the update does not inflate every dispatch future: the
        // dispatch state machine nests through reactions and callbacks, and
        // embedding this future inline overflowed the stack where the
        // previous tokio::spawn had heap-allocated it. Box::pin keeps that
        // heap allocation while preserving awaited-before-response order.
        Box::pin(
            async move {
                let started_at = Instant::now(); // determinism-ok: production-only projection duration metric
                crate::query_projection_metrics::record_update_enqueued(
                    &tenant,
                    &entity_type,
                    &operation,
                    "inline_dispatch_fallback",
                );
                crate::query_projection_metrics::record_update_started(
                    &tenant,
                    &entity_type,
                    &operation,
                    "inline_dispatch_fallback",
                );
                crate::query_projection_metrics::record_update_queue_wait(
                    &tenant,
                    &entity_type,
                    &operation,
                    "inline_dispatch_fallback",
                    started_at.duration_since(enqueued_at),
                );
                let result = if status == "Deleted" {
                    query_plane
                        .remove_projection(&tenant, &entity_type, &entity_id, sequence_nr)
                        .await
                } else {
                    query_plane
                        .upsert_projection(
                            &tenant,
                            &entity_type,
                            &entity_id,
                            &status,
                            &fields,
                            &projected_state,
                            sequence_nr,
                        )
                        .await
                };
                let result_label = if result.is_ok() { "ok" } else { "error" };
                crate::query_projection_metrics::record_update_duration(
                    &tenant,
                    &entity_type,
                    &operation,
                    "inline_dispatch_fallback",
                    result_label,
                    started_at.elapsed(),
                );
                crate::query_projection_metrics::record_update_end_to_end_duration(
                    &tenant,
                    &entity_type,
                    &operation,
                    "inline_dispatch_fallback",
                    result_label,
                    enqueued_at.elapsed(),
                );

                if let Err(e) = result {
                    crate::query_projection_metrics::record_update_error(
                        &tenant,
                        &entity_type,
                        &operation,
                        "inline_dispatch_fallback",
                    );
                    // The action is committed and non-idempotent, so the
                    // dispatch is still acknowledged — but the projection now
                    // serves a stale row until shadow/parity repair. Loud by
                    // convention (entity_ops create/patch fail their request
                    // on this same error); the ack-vs-fail tradeoff for
                    // dispatch is recorded in ADR-0142.
                    tracing::error!(
                        error = %e,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        operation = %operation,
                        "failed to update query projection for an acknowledged dispatch"
                    );
                } else {
                    crate::query_projection_metrics::record_update_applied_sequence(
                        &tenant,
                        &entity_type,
                        &operation,
                        "inline_dispatch_fallback",
                        sequence_nr,
                    );
                }
            }
            .instrument(span),
        )
        .await;
    }
}
