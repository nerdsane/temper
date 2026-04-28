//! Bounded background persistence for observe trajectory entries.

#[cfg(test)]
use std::sync::Mutex;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram},
};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::Instrument;

use crate::event_store::ServerEventStore;
use crate::state::trajectory::TrajectoryEntry;

const DEFAULT_CAPACITY: usize = 8_192;
const DRAIN_BATCH_LIMIT: usize = 128;

struct TrajectoryOutboxMetrics {
    outbox_depth: Gauge<u64>,
    outbox_capacity: Gauge<u64>,
    enqueued_total: Counter<u64>,
    dropped_total: Counter<u64>,
    batch_flush_ms: Histogram<f64>,
    persist_latency_ms: Histogram<f64>,
}

fn metrics() -> &'static TrajectoryOutboxMetrics {
    static METRICS: OnceLock<TrajectoryOutboxMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        TrajectoryOutboxMetrics {
            outbox_depth: meter
                .u64_gauge("temper_trajectory_outbox_depth")
                .with_description("Current number of trajectory entries waiting in the outbox.")
                .build(),
            outbox_capacity: meter
                .u64_gauge("temper_trajectory_outbox_capacity")
                .with_description("Configured capacity of the trajectory persistence outbox.")
                .build(),
            enqueued_total: meter
                .u64_counter("temper_trajectory_outbox_enqueued_total")
                .with_description("Trajectory entries accepted by the bounded persistence outbox.")
                .build(),
            dropped_total: meter
                .u64_counter("temper_trajectory_outbox_dropped_total")
                .with_description("Trajectory entries dropped because the persistence outbox was unavailable or full.")
                .build(),
            batch_flush_ms: meter
                .f64_histogram("temper_trajectory_outbox_batch_flush_ms")
                .with_unit("ms")
                .with_description("Wall time to drain and persist one trajectory outbox batch.")
                .build(),
            persist_latency_ms: meter
                .f64_histogram("temper_trajectory_outbox_persist_latency_ms")
                .with_unit("ms")
                .with_description("Wall time to persist a trajectory entry drained from the outbox.")
                .build(),
        }
    })
}

fn outbox_capacity() -> usize {
    static CAPACITY: OnceLock<usize> = OnceLock::new();
    *CAPACITY.get_or_init(|| {
        std::env::var("TEMPER_TRAJECTORY_OUTBOX_CAPACITY") // determinism-ok: read once at startup
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|capacity| *capacity > 0)
            .unwrap_or(DEFAULT_CAPACITY)
    })
}

fn entry_attrs(entry: &TrajectoryEntry, backend: &str) -> [KeyValue; 4] {
    [
        KeyValue::new("tenant", entry.tenant.clone()),
        KeyValue::new("entity_type", entry.entity_type.clone()),
        KeyValue::new("action", entry.action.clone()),
        KeyValue::new("backend", backend.to_string()),
    ]
}

fn record_enqueued(entry: &TrajectoryEntry, backend: &str) {
    metrics()
        .enqueued_total
        .add(1, &entry_attrs(entry, backend));
}

fn record_depth(depth: usize) {
    metrics().outbox_depth.record(depth as u64, &[]);
}

fn record_capacity(capacity: usize) {
    metrics().outbox_capacity.record(capacity as u64, &[]);
}

fn record_dropped(entry: &TrajectoryEntry, backend: &str, reason: &str) {
    let attrs = [
        KeyValue::new("tenant", entry.tenant.clone()),
        KeyValue::new("entity_type", entry.entity_type.clone()),
        KeyValue::new("action", entry.action.clone()),
        KeyValue::new("backend", backend.to_string()),
        KeyValue::new("reason", reason.to_string()),
    ];
    metrics().dropped_total.add(1, &attrs);
}

fn record_batch_flush(duration: Duration, batch_len: usize) {
    let attrs = [KeyValue::new("batch_len", batch_len as i64)];
    metrics()
        .batch_flush_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}

fn record_persist_latency(
    entry: &TrajectoryEntry,
    backend: &str,
    result: &str,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", entry.tenant.clone()),
        KeyValue::new("entity_type", entry.entity_type.clone()),
        KeyValue::new("action", entry.action.clone()),
        KeyValue::new("backend", backend.to_string()),
        KeyValue::new("result", result.to_string()),
    ];
    metrics()
        .persist_latency_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}

struct QueuedTrajectory {
    store: Option<Arc<ServerEventStore>>,
    entry: TrajectoryEntry,
}

pub(crate) struct TrajectoryOutbox {
    sender: mpsc::Sender<QueuedTrajectory>,
    depth: Arc<AtomicUsize>,
    dropped_total: Arc<AtomicU64>,
    #[cfg(test)]
    receiver_guard: Option<Mutex<mpsc::Receiver<QueuedTrajectory>>>,
}

impl TrajectoryOutbox {
    fn spawn(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        let depth = Arc::new(AtomicUsize::new(0));
        record_capacity(capacity);
        record_depth(0);
        tokio::spawn(drain(receiver, Arc::clone(&depth)));
        Self {
            sender,
            depth,
            dropped_total: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            receiver_guard: None,
        }
    }

    fn try_record(&self, store: Arc<ServerEventStore>, entry: TrajectoryEntry) -> bool {
        self.try_enqueue(Some(store), entry)
    }

    fn try_enqueue(&self, store: Option<Arc<ServerEventStore>>, entry: TrajectoryEntry) -> bool {
        let backend = store
            .as_ref()
            .map(|store| store.backend_name())
            .unwrap_or("test");
        let metric_entry = entry.clone();
        self.depth.fetch_add(1, Ordering::Relaxed);
        match self.sender.try_send(QueuedTrajectory { store, entry }) {
            Ok(()) => {
                record_enqueued(&metric_entry, backend);
                record_depth(self.depth.load(Ordering::Relaxed));
                true
            }
            Err(TrySendError::Full(item)) => {
                self.depth.fetch_sub(1, Ordering::Relaxed);
                self.dropped_total.fetch_add(1, Ordering::Relaxed);
                record_depth(self.depth.load(Ordering::Relaxed));
                record_dropped(&item.entry, backend, "outbox_full");
                tracing::warn!(
                    tenant = %item.entry.tenant,
                    entity_type = %item.entry.entity_type,
                    entity_id = %item.entry.entity_id,
                    action = %item.entry.action,
                    "trajectory outbox full; dropping entry"
                );
                false
            }
            Err(TrySendError::Closed(item)) => {
                self.depth.fetch_sub(1, Ordering::Relaxed);
                self.dropped_total.fetch_add(1, Ordering::Relaxed);
                record_depth(self.depth.load(Ordering::Relaxed));
                record_dropped(&item.entry, backend, "outbox_closed");
                tracing::error!(
                    tenant = %item.entry.tenant,
                    entity_type = %item.entry.entity_type,
                    entity_id = %item.entry.entity_id,
                    action = %item.entry.action,
                    "trajectory outbox closed; dropping entry"
                );
                false
            }
        }
    }

    #[cfg(test)]
    fn for_tests(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        record_capacity(capacity);
        record_depth(0);
        Self {
            sender,
            depth: Arc::new(AtomicUsize::new(0)),
            dropped_total: Arc::new(AtomicU64::new(0)),
            receiver_guard: Some(Mutex::new(receiver)),
        }
    }

    #[cfg(test)]
    fn try_record_for_test(&self, entry: TrajectoryEntry) -> bool {
        debug_assert!(self.receiver_guard.is_some());
        self.try_enqueue(None, entry)
    }

    #[cfg(test)]
    fn dropped_total(&self) -> u64 {
        self.dropped_total.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }
}

async fn drain(mut receiver: mpsc::Receiver<QueuedTrajectory>, depth: Arc<AtomicUsize>) {
    while let Some(first) = receiver.recv().await {
        let batch_started_at = Instant::now();
        let mut batch = Vec::with_capacity(DRAIN_BATCH_LIMIT.min(depth.load(Ordering::Relaxed)));
        batch.push(first);
        while batch.len() < DRAIN_BATCH_LIMIT {
            match receiver.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }

        let batch_len = batch.len();
        for item in batch {
            depth.fetch_sub(1, Ordering::Relaxed);
            record_depth(depth.load(Ordering::Relaxed));
            persist_drained(item).await;
        }
        record_batch_flush(batch_started_at.elapsed(), batch_len);
    }
}

async fn persist_drained(item: QueuedTrajectory) {
    let Some(store) = item.store else {
        return;
    };
    let backend = store.backend_name();
    let entry = item.entry;
    let span = tracing::info_span!(
        "trajectory_outbox.persist",
        tenant = %entry.tenant,
        entity_type = %entry.entity_type,
        entity_id = %entry.entity_id,
        action = %entry.action,
        backend = backend,
    );

    async move {
        let started_at = Instant::now();
        match store.persist_trajectory_entry(&entry).await {
            Ok(()) => {
                record_persist_latency(&entry, backend, "ok", started_at.elapsed());
            }
            Err(error) => {
                record_persist_latency(&entry, backend, "error", started_at.elapsed());
                tracing::error!(error = %error, "failed to persist trajectory entry from outbox");
            }
        }
    }
    .instrument(span)
    .await;
}

fn global() -> &'static TrajectoryOutbox {
    static OUTBOX: OnceLock<TrajectoryOutbox> = OnceLock::new();
    OUTBOX.get_or_init(|| TrajectoryOutbox::spawn(outbox_capacity()))
}

pub(crate) fn try_record(store: Arc<ServerEventStore>, entry: TrajectoryEntry) -> bool {
    global().try_record(store, entry)
}

impl crate::state::ServerState {
    pub(crate) fn enqueue_trajectory_entry(&self, entry: TrajectoryEntry) -> bool {
        let Some(store) = self.event_store.as_ref().cloned() else {
            return true;
        };
        try_record(store, entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

    fn entry(id: &str) -> TrajectoryEntry {
        TrajectoryEntry {
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            tenant: "tenant".to_string(),
            entity_type: "Session".to_string(),
            entity_id: id.to_string(),
            action: "ProgressMade".to_string(),
            success: true,
            from_status: Some("Running".to_string()),
            to_status: Some("Running".to_string()),
            error: None,
            agent_id: Some("agent".to_string()),
            session_id: Some("session".to_string()),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some(TrajectorySource::Entity),
            spec_governed: Some(true),
            agent_type: None,
            request_body: None,
            intent: None,
            matched_policy_ids: None,
        }
    }

    #[test]
    fn try_record_reports_drop_when_full() {
        let outbox = TrajectoryOutbox::for_tests(1);
        assert!(outbox.try_record_for_test(entry("one")));
        assert!(!outbox.try_record_for_test(entry("two")));
        assert_eq!(outbox.dropped_total(), 1);
        assert_eq!(outbox.depth(), 1);
    }
}
