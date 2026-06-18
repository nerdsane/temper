//! Bounded, retrying background persistence for full OTS trajectory artifacts.
#![cfg_attr(not(feature = "observe"), allow(dead_code))]

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram},
};
use temper_runtime::persistence::PersistenceError;
use temper_store_turso::OtsTrajectoryParams;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::storage::{MetadataStore, OtsStore};

const DEFAULT_CAPACITY: usize = 512;
const DEFAULT_DRAIN_BATCH: usize = 16;
const DEFAULT_MAX_ATTEMPTS: u32 = 5;
const DEFAULT_RETRY_DELAY_MS: u64 = 100;

struct OtsOutboxMetrics {
    depth: Gauge<u64>,
    capacity: Gauge<u64>,
    enqueued_total: Counter<u64>,
    rejected_total: Counter<u64>,
    retry_total: Counter<u64>,
    persisted_total: Counter<u64>,
    failed_total: Counter<u64>,
    persist_latency_ms: Histogram<f64>,
}

fn metrics() -> &'static OtsOutboxMetrics {
    static METRICS: OnceLock<OtsOutboxMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        OtsOutboxMetrics {
            depth: meter
                .u64_gauge("temper_ots_trajectory_outbox_depth")
                .with_description("Current number of OTS trajectory artifacts pending persistence.")
                .build(),
            capacity: meter
                .u64_gauge("temper_ots_trajectory_outbox_capacity")
                .with_description("Configured OTS trajectory persistence outbox capacity.")
                .build(),
            enqueued_total: meter
                .u64_counter("temper_ots_trajectory_outbox_enqueued_total")
                .with_description("OTS trajectory artifacts accepted by the persistence outbox.")
                .build(),
            rejected_total: meter
                .u64_counter("temper_ots_trajectory_outbox_rejected_total")
                .with_description("OTS trajectory artifacts rejected because the outbox was full.")
                .build(),
            retry_total: meter
                .u64_counter("temper_ots_trajectory_outbox_retry_total")
                .with_description("OTS trajectory persistence attempts scheduled for retry.")
                .build(),
            persisted_total: meter
                .u64_counter("temper_ots_trajectory_outbox_persisted_total")
                .with_description("OTS trajectory artifacts persisted by the outbox.")
                .build(),
            failed_total: meter
                .u64_counter("temper_ots_trajectory_outbox_failed_total")
                .with_description("OTS trajectory artifacts that exhausted outbox retries.")
                .build(),
            persist_latency_ms: meter
                .f64_histogram("temper_ots_trajectory_outbox_persist_latency_ms")
                .with_unit("ms")
                .with_description("Wall time for one OTS trajectory persistence attempt.")
                .build(),
        }
    })
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name) // determinism-ok: observe persistence queue config read at startup
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name) // determinism-ok: observe persistence queue config read at startup
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name) // determinism-ok: observe persistence queue config read at startup
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn outbox_config() -> OtsTrajectoryOutboxConfig {
    OtsTrajectoryOutboxConfig {
        capacity: env_usize("TEMPER_OTS_TRAJECTORY_OUTBOX_CAPACITY", DEFAULT_CAPACITY),
        drain_batch: env_usize("TEMPER_OTS_TRAJECTORY_DRAIN_BATCH", DEFAULT_DRAIN_BATCH),
        max_attempts: env_u32("TEMPER_OTS_TRAJECTORY_MAX_ATTEMPTS", DEFAULT_MAX_ATTEMPTS),
        retry_delay: Duration::from_millis(env_u64(
            "TEMPER_OTS_TRAJECTORY_RETRY_DELAY_MS",
            DEFAULT_RETRY_DELAY_MS,
        )),
    }
}

fn attrs(item: &OtsTrajectoryWrite, backend: &'static str) -> [KeyValue; 3] {
    [
        KeyValue::new("tenant", item.tenant.clone()),
        KeyValue::new("outcome", item.outcome.clone()),
        KeyValue::new("backend", backend.to_string()),
    ]
}

fn record_depth(depth: usize) {
    metrics().depth.record(depth as u64, &[]);
}

fn record_capacity(capacity: usize) {
    metrics().capacity.record(capacity as u64, &[]);
}

fn record_enqueue(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().enqueued_total.add(1, &attrs(item, backend));
}

fn record_rejected(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().rejected_total.add(1, &attrs(item, backend));
}

fn record_retry(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().retry_total.add(1, &attrs(item, backend));
}

fn record_persisted(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().persisted_total.add(1, &attrs(item, backend));
}

fn record_failed(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().failed_total.add(1, &attrs(item, backend));
}

fn record_persist_latency(
    item: &OtsTrajectoryWrite,
    backend: &'static str,
    result: &str,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", item.tenant.clone()),
        KeyValue::new("outcome", item.outcome.clone()),
        KeyValue::new("backend", backend.to_string()),
        KeyValue::new("result", result.to_string()),
    ];
    metrics()
        .persist_latency_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}

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

    pub(crate) fn try_enqueue_metadata_store(
        &self,
        backend: &'static str,
        store: Arc<dyn MetadataStore>,
        item: OtsTrajectoryWrite,
    ) -> Result<(), OtsTrajectoryEnqueueError> {
        self.try_enqueue(backend, Arc::new(MetadataOtsStore { inner: store }), item)
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
            let started_at = Instant::now();
            match queued
                .store
                .persist_ots_trajectory(&queued.item.params())
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
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.inner.get_ots_trajectory(trajectory_id).await
    }
}

#[cfg(test)]
mod tests;
