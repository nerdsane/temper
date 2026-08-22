//! Bounded, retrying background persistence for full OTS trajectory artifacts.
#![cfg_attr(not(feature = "observe"), allow(dead_code))]

use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use temper_runtime::persistence::PersistenceError;
use temper_store_turso::OtsTrajectoryParams;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::storage::{MetadataStore, MetadataStoreProvider, OtsStore};

mod config;
mod metrics;

use config::outbox_config;
use metrics::{
    record_capacity, record_depth, record_enqueue, record_failed, record_persist_latency,
    record_persisted, record_rejected, record_retry,
};

#[derive(Clone)]
struct OtsTrajectoryOutboxConfig {
    capacity: usize,
    drain_batch: usize,
    max_attempts: u32,
    retry_delay: Duration,
}

/// Owned OTS trajectory artifact ready for background persistence.
#[derive(Clone, Debug)]
pub(crate) struct OtsTrajectoryWrite {
    pub trajectory_id: String,
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub outcome: String,
    pub turn_count: i64,
    pub data: String,
}

impl OtsTrajectoryWrite {
    fn params(&self) -> OtsTrajectoryParams<'_> {
        OtsTrajectoryParams {
            trajectory_id: &self.trajectory_id,
            tenant: &self.tenant,
            agent_id: &self.agent_id,
            session_id: &self.session_id,
            outcome: &self.outcome,
            turn_count: self.turn_count,
            data: &self.data,
        }
    }
}

struct QueuedOtsTrajectory {
    store: Arc<dyn OtsStore>,
    backend: &'static str,
    item: OtsTrajectoryWrite,
}

/// Bounded queue for OTS trajectory artifacts.
pub(crate) struct OtsTrajectoryOutbox {
    sender: mpsc::Sender<QueuedOtsTrajectory>,
    config: OtsTrajectoryOutboxConfig,
    depth: Arc<AtomicUsize>,
    rejected_total: Arc<AtomicU64>,
    #[cfg(test)]
    failed_total: Arc<AtomicU64>,
}

/// Rejection reason for OTS trajectory enqueue attempts.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum OtsTrajectoryEnqueueError {
    Full,
    Closed,
}

impl OtsTrajectoryOutbox {
    /// Start the OTS outbox with production configuration.
    pub(crate) fn start() -> Arc<Self> {
        Self::start_with_config(outbox_config())
    }

    fn start_with_config(config: OtsTrajectoryOutboxConfig) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(config.capacity);
        let depth = Arc::new(AtomicUsize::new(0));
        let failed_total = Arc::new(AtomicU64::new(0));
        let outbox = Arc::new(Self {
            sender,
            config: config.clone(),
            depth: Arc::clone(&depth),
            rejected_total: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            failed_total: Arc::clone(&failed_total),
        });
        record_capacity(config.capacity);
        record_depth(0);
        tokio::spawn(run_worker(receiver, depth, failed_total, config)); // determinism-ok: external observe persistence
        outbox
    }

    pub(crate) async fn try_enqueue_metadata_store(
        &self,
        backend: &'static str,
        store: Arc<dyn MetadataStore>,
        item: OtsTrajectoryWrite,
    ) -> Result<(), OtsTrajectoryEnqueueError> {
        let store = Arc::new(MetadataOtsStore { inner: store });
        store
            .enqueue_ots_trajectory(&item.params())
            .await
            .map_err(|error| {
                tracing::warn!(
                    tenant = %item.tenant,
                    trajectory_id = %item.trajectory_id,
                    error = %error,
                    "failed to durably enqueue OTS trajectory"
                );
                OtsTrajectoryEnqueueError::Closed
            })?;
        self.try_enqueue(backend, store, item)
    }

    pub(crate) fn recover_queued_metadata_stores(
        self: &Arc<Self>,
        backend: &'static str,
        provider: Arc<dyn MetadataStoreProvider>,
    ) {
        let outbox = Arc::clone(self);
        tokio::spawn(async move { outbox.recover_queued(backend, provider).await }); // determinism-ok: external observe persistence recovery
    }

    async fn recover_queued(
        self: Arc<Self>,
        backend: &'static str,
        provider: Arc<dyn MetadataStoreProvider>,
    ) {
        let stores = provider.all_stores().await;
        for store in stores {
            let rows = match store.list_queued_ots_trajectories(1024).await {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(error = %error, "failed to load queued OTS trajectories for recovery");
                    continue;
                }
            };
            for row in rows {
                let item = OtsTrajectoryWrite {
                    trajectory_id: row.trajectory_id,
                    tenant: row.tenant,
                    agent_id: row.agent_id,
                    session_id: row.session_id,
                    outcome: row.outcome,
                    turn_count: row.turn_count,
                    data: row.data,
                };
                if let Err(error) = self.try_enqueue_recovered(
                    backend,
                    Arc::new(MetadataOtsStore {
                        inner: store.clone(),
                    }),
                    item,
                ) {
                    tracing::warn!(
                        ?error,
                        "OTS trajectory recovery queue full or closed; row remains durable"
                    );
                    break;
                }
            }
        }
    }

    fn try_enqueue_recovered(
        &self,
        backend: &'static str,
        store: Arc<dyn OtsStore>,
        item: OtsTrajectoryWrite,
    ) -> Result<(), OtsTrajectoryEnqueueError> {
        self.try_enqueue(backend, store, item)
    }

    fn try_enqueue(
        &self,
        backend: &'static str,
        store: Arc<dyn OtsStore>,
        item: OtsTrajectoryWrite,
    ) -> Result<(), OtsTrajectoryEnqueueError> {
        let prev = self.depth.fetch_add(1, Ordering::Relaxed);
        if prev >= self.config.capacity {
            self.depth.fetch_sub(1, Ordering::Relaxed);
            self.rejected_total.fetch_add(1, Ordering::Relaxed);
            record_depth(self.depth.load(Ordering::Relaxed));
            record_rejected(&item, backend);
            tracing::warn!(
                tenant = %item.tenant,
                trajectory_id = %item.trajectory_id,
                agent_id = %item.agent_id,
                session_id = %item.session_id,
                "OTS trajectory outbox full; rejecting upload for retry"
            );
            return Err(OtsTrajectoryEnqueueError::Full);
        }

        let metric_item = item.clone();
        let queued = QueuedOtsTrajectory {
            store,
            backend,
            item,
        };
        match self.sender.try_send(queued) {
            Ok(()) => {
                record_enqueue(&metric_item, backend);
                record_depth(self.depth.load(Ordering::Relaxed));
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(queued)) => {
                self.depth.fetch_sub(1, Ordering::Relaxed);
                self.rejected_total.fetch_add(1, Ordering::Relaxed);
                record_rejected(&queued.item, backend);
                record_depth(self.depth.load(Ordering::Relaxed));
                Err(OtsTrajectoryEnqueueError::Full)
            }
            Err(mpsc::error::TrySendError::Closed(queued)) => {
                self.depth.fetch_sub(1, Ordering::Relaxed);
                record_failed(&queued.item, backend);
                record_depth(self.depth.load(Ordering::Relaxed));
                Err(OtsTrajectoryEnqueueError::Closed)
            }
        }
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn rejected_total(&self) -> u64 {
        self.rejected_total.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn failed_total(&self) -> u64 {
        self.failed_total.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn start_for_tests(
        capacity: usize,
        drain_batch: usize,
        max_attempts: u32,
        retry_delay: Duration,
    ) -> Arc<Self> {
        Self::start_with_config(OtsTrajectoryOutboxConfig {
            capacity,
            drain_batch,
            max_attempts,
            retry_delay,
        })
    }

    #[cfg(test)]
    fn try_enqueue_for_tests(
        &self,
        store: Arc<dyn OtsStore>,
        item: OtsTrajectoryWrite,
    ) -> Result<(), OtsTrajectoryEnqueueError> {
        self.try_enqueue("test", store, item)
    }

    #[cfg(test)]
    async fn try_enqueue_durable_for_tests(
        &self,
        store: Arc<dyn OtsStore>,
        item: OtsTrajectoryWrite,
    ) -> Result<(), OtsTrajectoryEnqueueError> {
        store
            .enqueue_ots_trajectory(&item.params())
            .await
            .map_err(|_| OtsTrajectoryEnqueueError::Closed)?;
        self.try_enqueue("test", store, item)
    }
}

async fn run_worker(
    mut receiver: mpsc::Receiver<QueuedOtsTrajectory>,
    depth: Arc<AtomicUsize>,
    failed_total: Arc<AtomicU64>,
    config: OtsTrajectoryOutboxConfig,
) {
    while let Some(first) = receiver.recv().await {
        let mut batch = Vec::with_capacity(config.drain_batch);
        batch.push(first);
        while batch.len() < config.drain_batch {
            match receiver.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }
        for item in batch {
            persist_with_retries(item, &config, &failed_total).await;
            depth.fetch_sub(1, Ordering::Relaxed);
            record_depth(depth.load(Ordering::Relaxed));
        }
    }
}

async fn persist_with_retries(
    queued: QueuedOtsTrajectory,
    config: &OtsTrajectoryOutboxConfig,
    failed_total: &AtomicU64,
) {
    let span = tracing::info_span!(
        "ots_trajectory_outbox.persist",
        tenant = %queued.item.tenant,
        trajectory_id = %queued.item.trajectory_id,
        agent_id = %queued.item.agent_id,
        session_id = %queued.item.session_id,
        backend = queued.backend,
    );
    async move {
        let mut attempt = 1;
        loop {
            let started_at = Instant::now(); // determinism-ok: production outbox latency metric only
            match queued
                .store
                .mark_ots_trajectory_persisted(&queued.item.tenant, &queued.item.trajectory_id)
                .await
            {
                Ok(()) => {
                    record_persist_latency(
                        &queued.item,
                        queued.backend,
                        "ok",
                        started_at.elapsed(),
                    );
                    record_persisted(&queued.item, queued.backend);
                    tracing::info!(
                        trajectory_id = %queued.item.trajectory_id,
                        agent_id = %queued.item.agent_id,
                        turn_count = queued.item.turn_count,
                        outcome = %queued.item.outcome,
                        attempts = attempt,
                        "ots.trajectory.persisted"
                    );
                    return;
                }
                Err(error) if attempt < config.max_attempts => {
                    record_persist_latency(
                        &queued.item,
                        queued.backend,
                        "retry",
                        started_at.elapsed(),
                    );
                    record_retry(&queued.item, queued.backend);
                    tracing::warn!(
                        error = %error,
                        attempt = attempt,
                        max_attempts = config.max_attempts,
                        "OTS trajectory persistence failed; retrying"
                    );
                    attempt += 1;
                    tokio::time::sleep(config.retry_delay).await;
                }
                Err(error) => {
                    record_persist_latency(
                        &queued.item,
                        queued.backend,
                        "failed",
                        started_at.elapsed(),
                    );
                    record_failed(&queued.item, queued.backend);
                    failed_total.fetch_add(1, Ordering::Relaxed);
                    if let Err(mark_error) = queued
                        .store
                        .mark_ots_trajectory_failed(
                            &queued.item.tenant,
                            &queued.item.trajectory_id,
                            &error.to_string(),
                        )
                        .await
                    {
                        tracing::error!(
                            error = %mark_error,
                            "failed to mark OTS trajectory outbox row as failed"
                        );
                    }
                    tracing::error!(
                        error = %error,
                        attempts = attempt,
                        "OTS trajectory persistence exhausted retries"
                    );
                    return;
                }
            }
        }
    }
    .instrument(span)
    .await;
}

struct MetadataOtsStore {
    inner: Arc<dyn MetadataStore>,
}

#[async_trait::async_trait]
impl OtsStore for MetadataOtsStore {
    async fn persist_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.inner.persist_ots_trajectory(params).await
    }

    async fn enqueue_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.inner.enqueue_ots_trajectory(params).await
    }

    async fn mark_ots_trajectory_persisted(
        &self,
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        self.inner
            .mark_ots_trajectory_persisted(tenant, trajectory_id)
            .await
    }

    async fn mark_ots_trajectory_failed(
        &self,
        tenant: &str,
        trajectory_id: &str,
        error: &str,
    ) -> Result<(), PersistenceError> {
        self.inner
            .mark_ots_trajectory_failed(tenant, trajectory_id, error)
            .await
    }

    async fn list_queued_ots_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<temper_store_turso::OtsQueuedTrajectoryRow>, PersistenceError> {
        self.inner.list_queued_ots_trajectories(limit).await
    }

    async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<temper_store_turso::OtsTrajectoryRow>, PersistenceError> {
        self.inner
            .list_ots_trajectories(tenant, agent_id, outcome, limit)
            .await
    }

    async fn get_ots_trajectory(
        &self,
        tenant: &str,
        trajectory_id: &str,
    ) -> Result<Option<temper_store_turso::OtsTrajectoryDocument>, PersistenceError> {
        self.inner.get_ots_trajectory(tenant, trajectory_id).await
    }
}

#[cfg(test)]
mod tests;
