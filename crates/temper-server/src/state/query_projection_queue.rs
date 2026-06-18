//! Bounded, coalescing query-projection writer.
//!
//! The event journal remains the synchronous commit point. This worker keeps
//! derived query-plane rows off the dispatch critical path while preserving a
//! per-entity latest-sequence contract before opening a storage connection.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::Notify;
use tracing::Instrument;

use crate::storage::QueryPlaneStore;

const DEFAULT_PROJECTION_QUEUE_CAPACITY: usize = 20_000;
const DEFAULT_PROJECTION_DRAIN_BATCH: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ProjectionKey {
    tenant: String,
    entity_type: String,
    entity_id: String,
}

#[derive(Clone, Debug)]
enum ProjectionOperation {
    Upsert {
        status: String,
        fields: serde_json::Value,
        state: serde_json::Value,
    },
    Remove,
}

#[derive(Clone, Debug)]
struct QueuedProjectionUpdate {
    key: ProjectionKey,
    operation: ProjectionOperation,
    sequence_nr: u64,
    enqueued_at: Instant,
    source: &'static str,
}

#[derive(Debug, Default)]
struct PendingProjectionUpdates {
    updates: BTreeMap<ProjectionKey, QueuedProjectionUpdate>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ProjectionEnqueueOutcome {
    Enqueued,
    Coalesced,
    StaleSkipped,
    Full,
}

/// Background projection writer shared by all `ServerState` clones.
pub(crate) struct QueryProjectionWriteQueue {
    store: Arc<dyn QueryPlaneStore>,
    pending: Arc<Mutex<PendingProjectionUpdates>>,
    notify: Arc<Notify>,
    capacity: usize,
    drain_batch: usize,
}

impl QueryProjectionWriteQueue {
    pub(crate) fn start(store: Arc<dyn QueryPlaneStore>) -> Arc<Self> {
        let queue = Arc::new(Self {
            store,
            pending: Arc::new(Mutex::new(PendingProjectionUpdates::default())),
            notify: Arc::new(Notify::new()),
            capacity: projection_queue_capacity(),
            drain_batch: projection_drain_batch(),
        });
        queue.spawn_worker();
        queue
    }

    #[cfg(test)]
    fn new_for_test(store: Arc<dyn QueryPlaneStore>, capacity: usize, drain_batch: usize) -> Self {
        Self {
            store,
            pending: Arc::new(Mutex::new(PendingProjectionUpdates::default())),
            notify: Arc::new(Notify::new()),
            capacity,
            drain_batch,
        }
    }

    #[expect(clippy::too_many_arguments, reason = "projection enqueue boundary")]
    pub(crate) fn enqueue_upsert(
        &self,
        tenant: String,
        entity_type: String,
        entity_id: String,
        status: String,
        fields: serde_json::Value,
        state: serde_json::Value,
        sequence_nr: u64,
        source: &'static str,
    ) -> ProjectionEnqueueOutcome {
        self.enqueue(QueuedProjectionUpdate {
            key: ProjectionKey {
                tenant,
                entity_type,
                entity_id,
            },
            operation: ProjectionOperation::Upsert {
                status,
                fields,
                state,
            },
            sequence_nr,
            enqueued_at: Instant::now(),
            source,
        })
    }

    pub(crate) fn enqueue_remove(
        &self,
        tenant: String,
        entity_type: String,
        entity_id: String,
        sequence_nr: u64,
        source: &'static str,
    ) -> ProjectionEnqueueOutcome {
        self.enqueue(QueuedProjectionUpdate {
            key: ProjectionKey {
                tenant,
                entity_type,
                entity_id,
            },
            operation: ProjectionOperation::Remove,
            sequence_nr,
            enqueued_at: Instant::now(),
            source,
        })
    }

    fn enqueue(&self, update: QueuedProjectionUpdate) -> ProjectionEnqueueOutcome {
        let mut pending = self
            .pending
            .lock()
            .expect("query projection queue mutex poisoned");
        let existing = pending.updates.get(&update.key);
        if existing.is_some_and(|existing| existing.sequence_nr >= update.sequence_nr) {
            crate::query_projection_metrics::record_update_stale_skipped(
                &update.key.tenant,
                &update.key.entity_type,
                update.operation.label(),
                update.source,
            );
            return ProjectionEnqueueOutcome::StaleSkipped;
        }

        if existing.is_none() && pending.updates.len() >= self.capacity {
            crate::query_projection_metrics::record_update_dropped(
                &update.key.tenant,
                &update.key.entity_type,
                update.operation.label(),
                update.source,
            );
            return ProjectionEnqueueOutcome::Full;
        }

        let outcome = if existing.is_some() {
            ProjectionEnqueueOutcome::Coalesced
        } else {
            ProjectionEnqueueOutcome::Enqueued
        };
        match outcome {
            ProjectionEnqueueOutcome::Coalesced => {
                crate::query_projection_metrics::record_update_coalesced(
                    &update.key.tenant,
                    &update.key.entity_type,
                    update.operation.label(),
                    update.source,
                )
            }
            ProjectionEnqueueOutcome::Enqueued
            | ProjectionEnqueueOutcome::StaleSkipped
            | ProjectionEnqueueOutcome::Full => {}
        }
        pending.updates.insert(update.key.clone(), update);
        crate::query_projection_metrics::record_update_queue_depth(pending.updates.len() as u64);
        drop(pending);

        self.notify.notify_one();
        outcome
    }

    fn spawn_worker(self: &Arc<Self>) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            queue.run().await;
        });
    }

    async fn run(self: Arc<Self>) {
        loop {
            let updates = self.take_batch();
            if updates.is_empty() {
                self.notify.notified().await;
                continue;
            }

            for update in updates {
                self.apply(update).await;
            }
        }
    }

    fn take_batch(&self) -> Vec<QueuedProjectionUpdate> {
        let mut pending = self
            .pending
            .lock()
            .expect("query projection queue mutex poisoned");
        let mut updates = Vec::with_capacity(self.drain_batch.min(pending.updates.len()));
        for _ in 0..self.drain_batch {
            let Some((_, update)) = pending.updates.pop_first() else {
                break;
            };
            updates.push(update);
        }
        crate::query_projection_metrics::record_update_queue_depth(pending.updates.len() as u64);
        updates
    }

    async fn apply(&self, update: QueuedProjectionUpdate) {
        let operation = match update.operation {
            ProjectionOperation::Upsert { .. } => "upsert",
            ProjectionOperation::Remove => "remove",
        };
        let source = update.source;
        let span = tracing::info_span!(
            "dispatch.phase.query_projection.queued",
            otel.name = "dispatch.phase.query_projection.queued",
            tenant = %update.key.tenant,
            entity_type = %update.key.entity_type,
            entity_id = %update.key.entity_id,
            operation,
            sequence_nr = update.sequence_nr,
        );

        async move {
            let started_at = Instant::now();
            crate::query_projection_metrics::record_update_started(
                &update.key.tenant,
                &update.key.entity_type,
                operation,
                source,
            );
            crate::query_projection_metrics::record_update_queue_wait(
                &update.key.tenant,
                &update.key.entity_type,
                operation,
                source,
                started_at.duration_since(update.enqueued_at),
            );

            let result = match update.operation {
                ProjectionOperation::Upsert {
                    status,
                    fields,
                    state,
                } => {
                    self.store
                        .upsert_projection(
                            &update.key.tenant,
                            &update.key.entity_type,
                            &update.key.entity_id,
                            &status,
                            &fields,
                            &state,
                            update.sequence_nr,
                        )
                        .await
                }
                ProjectionOperation::Remove => {
                    self.store
                        .remove_projection(
                            &update.key.tenant,
                            &update.key.entity_type,
                            &update.key.entity_id,
                        )
                        .await
                }
            };
            let result_label = if result.is_ok() { "ok" } else { "error" };
            crate::query_projection_metrics::record_update_duration(
                &update.key.tenant,
                &update.key.entity_type,
                operation,
                source,
                result_label,
                started_at.elapsed(),
            );
            crate::query_projection_metrics::record_update_end_to_end_duration(
                &update.key.tenant,
                &update.key.entity_type,
                operation,
                source,
                result_label,
                update.enqueued_at.elapsed(),
            );

            if let Err(e) = result {
                crate::query_projection_metrics::record_update_error(
                    &update.key.tenant,
                    &update.key.entity_type,
                    operation,
                    source,
                );
                tracing::error!(
                    error = %e,
                    tenant = %update.key.tenant,
                    entity_type = %update.key.entity_type,
                    entity_id = %update.key.entity_id,
                    operation,
                    sequence_nr = update.sequence_nr,
                    "failed to update queued query projection"
                );
            } else {
                crate::query_projection_metrics::record_update_applied_sequence(
                    &update.key.tenant,
                    &update.key.entity_type,
                    operation,
                    source,
                    update.sequence_nr,
                );
            }
        }
        .instrument(span)
        .await;
    }
}

impl ProjectionOperation {
    fn label(&self) -> &'static str {
        match self {
            ProjectionOperation::Upsert { .. } => "upsert",
            ProjectionOperation::Remove => "remove",
        }
    }
}

fn projection_queue_capacity() -> usize {
    std::env::var("TEMPER_QUERY_PROJECTION_QUEUE_CAPACITY") // determinism-ok: production side-effect queue sizing
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PROJECTION_QUEUE_CAPACITY)
}

fn projection_drain_batch() -> usize {
    std::env::var("TEMPER_QUERY_PROJECTION_DRAIN_BATCH") // determinism-ok: production side-effect queue sizing
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PROJECTION_DRAIN_BATCH)
}

#[cfg(test)]
mod tests;
