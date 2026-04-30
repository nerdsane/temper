//! Bounded background persistence for observe trajectory entries.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram},
};
use tracing::Instrument;

use crate::state::trajectory::TrajectoryEntry;
use crate::storage::TrajectorySink;

const DEFAULT_CAPACITY: usize = 8_192;

struct TrajectoryOutboxMetrics {
    outbox_depth: Gauge<u64>,
    outbox_capacity: Gauge<u64>,
    enqueued_total: Counter<u64>,
    dropped_total: Counter<u64>,
    persist_latency_ms: Histogram<f64>,
}

fn metrics() -> &'static TrajectoryOutboxMetrics {
    static METRICS: OnceLock<TrajectoryOutboxMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        TrajectoryOutboxMetrics {
            outbox_depth: meter
                .u64_gauge("temper_trajectory_outbox_depth")
                .with_description("Current number of trajectory entries in flight in the outbox.")
                .build(),
            outbox_capacity: meter
                .u64_gauge("temper_trajectory_outbox_capacity")
                .with_description("Configured maximum in-flight depth of the trajectory persistence outbox.")
                .build(),
            enqueued_total: meter
                .u64_counter("temper_trajectory_outbox_enqueued_total")
                .with_description("Trajectory entries accepted by the bounded persistence outbox.")
                .build(),
            dropped_total: meter
                .u64_counter("temper_trajectory_outbox_dropped_total")
                .with_description("Trajectory entries dropped because the persistence outbox was unavailable or full.")
                .build(),
            persist_latency_ms: meter
                .f64_histogram("temper_trajectory_outbox_persist_latency_ms")
                .with_unit("ms")
                .with_description("Wall time to persist a single trajectory entry from the outbox.")
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
    sink: Option<Arc<dyn TrajectorySink>>,
    backend: &'static str,
    entry: TrajectoryEntry,
}

pub(crate) struct TrajectoryOutbox {
    capacity: usize,
    depth: Arc<AtomicUsize>,
    dropped_total: Arc<AtomicU64>,
    #[cfg(test)]
    inflight: Option<Arc<tokio::sync::Notify>>,
}

impl TrajectoryOutbox {
    fn spawn(capacity: usize) -> Self {
        record_capacity(capacity);
        record_depth(0);
        Self {
            capacity,
            depth: Arc::new(AtomicUsize::new(0)),
            dropped_total: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            inflight: None,
        }
    }

    fn try_record(
        &self,
        backend: &'static str,
        sink: Arc<dyn TrajectorySink>,
        entry: TrajectoryEntry,
    ) -> bool {
        self.try_enqueue(Some(sink), backend, entry)
    }

    fn try_enqueue(
        &self,
        sink: Option<Arc<dyn TrajectorySink>>,
        backend: &'static str,
        entry: TrajectoryEntry,
    ) -> bool {
        let metric_entry = entry.clone();
        // Backpressure: cap the in-flight depth at `capacity`. Drop-newest on
        // overflow so a slow tenant DB cannot consume unbounded memory.
        let prev = self.depth.fetch_add(1, Ordering::Relaxed);
        if prev >= self.capacity {
            self.depth.fetch_sub(1, Ordering::Relaxed);
            self.dropped_total.fetch_add(1, Ordering::Relaxed);
            record_depth(self.depth.load(Ordering::Relaxed));
            record_dropped(&metric_entry, backend, "outbox_full");
            tracing::warn!(
                tenant = %metric_entry.tenant,
                entity_type = %metric_entry.entity_type,
                entity_id = %metric_entry.entity_id,
                action = %metric_entry.action,
                "trajectory outbox full; dropping entry"
            );
            return false;
        }
        record_enqueued(&metric_entry, backend);
        record_depth(self.depth.load(Ordering::Relaxed));

        // Spawn the persist on the current runtime so it follows the calling
        // task's lifecycle. This avoids the cross-runtime dead-drainer hazard
        // a single global channel-and-task would have under per-test tokio
        // runtimes.
        let depth = Arc::clone(&self.depth);
        let item = QueuedTrajectory {
            sink,
            backend,
            entry,
        };
        // In unit tests built via `for_tests`, skip the spawn so the bounded
        // depth/drop semantics can be exercised without a tokio runtime.
        #[cfg(test)]
        if self.inflight.is_some() {
            return true;
        }
        tokio::spawn(async move {
            persist_drained(item).await;
            depth.fetch_sub(1, Ordering::Relaxed);
            record_depth(depth.load(Ordering::Relaxed));
        });
        true
    }

    #[cfg(test)]
    fn for_tests(capacity: usize) -> Self {
        record_capacity(capacity);
        record_depth(0);
        Self {
            capacity,
            depth: Arc::new(AtomicUsize::new(0)),
            dropped_total: Arc::new(AtomicU64::new(0)),
            inflight: Some(Arc::new(tokio::sync::Notify::new())),
        }
    }

    #[cfg(test)]
    fn try_record_for_test(&self, entry: TrajectoryEntry) -> bool {
        debug_assert!(self.inflight.is_some());
        self.try_enqueue(None, "test", entry)
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

async fn persist_drained(item: QueuedTrajectory) {
    let Some(sink) = item.sink else {
        return;
    };
    let backend = item.backend;
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
        match sink.persist_trajectory_entry(&entry).await {
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

pub(crate) fn try_record(
    backend: &'static str,
    sink: Arc<dyn TrajectorySink>,
    entry: TrajectoryEntry,
) -> bool {
    global().try_record(backend, sink, entry)
}

impl crate::state::ServerState {
    pub(crate) fn enqueue_trajectory_entry(&self, entry: TrajectoryEntry) -> bool {
        let Some((backend, sink)) = self.trajectory_sink() else {
            return true;
        };
        try_record(backend, sink, entry)
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
