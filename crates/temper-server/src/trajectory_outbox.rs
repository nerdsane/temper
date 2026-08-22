//! Bounded background persistence for observe trajectory entries.
//!
//! The queue is bounded and its writes can fail, so this path can lose a
//! captured entry. A loss that only shows up as a log line lets a run with
//! holes in it later pass a conformance check, so every loss is counted and —
//! once per session — written to storage as a marker row the checker reads as
//! an evidence gap (`crate::conformance::CAPTURE_LOSS_ENTITY_TYPE`).

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

mod capture_loss;

pub(crate) use capture_loss::CaptureHealth;
use capture_loss::record_capture_loss;
#[cfg(test)]
use capture_loss::{
    MAX_MARKED_SESSIONS, MarkerClaim, claim_capture_loss_marker, claim_in,
    persist_capture_loss_marker, queued_marker_for_test, release_capture_loss_marker,
};

const DEFAULT_CAPACITY: usize = 8_192;

struct TrajectoryOutboxMetrics {
    outbox_depth: Gauge<u64>,
    outbox_capacity: Gauge<u64>,
    enqueued_total: Counter<u64>,
    dropped_total: Counter<u64>,
    capture_loss_marker_total: Counter<u64>,
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
                .with_description("Captured trajectory entries that never reached storage, by reason: the outbox was full or unavailable, or the write failed.")
                .build(),
            capture_loss_marker_total: meter
                .u64_counter("temper_trajectory_capture_loss_marker_total")
                .with_description("Attempts to write the per-session marker that records a capture loss, by result.")
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
    health: CaptureHealth,
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
        health: CaptureHealth,
    ) -> bool {
        self.try_enqueue(Some(sink), backend, entry, health)
    }

    fn try_enqueue(
        &self,
        sink: Option<Arc<dyn TrajectorySink>>,
        backend: &'static str,
        entry: TrajectoryEntry,
        health: CaptureHealth,
    ) -> bool {
        let metric_entry = entry.clone();
        // Backpressure: cap the in-flight depth at `capacity`. Drop-newest on
        // overflow so a slow tenant DB cannot consume unbounded memory.
        let prev = self.depth.fetch_add(1, Ordering::Relaxed);
        if prev >= self.capacity {
            self.depth.fetch_sub(1, Ordering::Relaxed);
            self.dropped_total.fetch_add(1, Ordering::Relaxed);
            record_depth(self.depth.load(Ordering::Relaxed));
            record_capture_loss(sink.clone(), backend, &metric_entry, "outbox_full", &health);
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
            health,
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
        self.try_enqueue(None, "test", entry, CaptureHealth::default())
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
    let health = item.health;
    let span = tracing::info_span!(
        "trajectory_outbox.persist",
        tenant = %entry.tenant,
        entity_type = %entry.entity_type,
        entity_id = %entry.entity_id,
        action = %entry.action,
        backend = backend,
    );

    async move {
        let started_at = Instant::now(); // determinism-ok: production trajectory sink latency only
        match sink.persist_trajectory_entry(&entry).await {
            Ok(()) => {
                record_persist_latency(&entry, backend, "ok", started_at.elapsed());
            }
            Err(error) => {
                record_persist_latency(&entry, backend, "error", started_at.elapsed());
                tracing::error!(error = %error, "failed to persist trajectory entry from outbox");
                // A write that failed loses the entry exactly as surely as a
                // full queue does. Both go through one place, so neither can
                // be the one that stays silent.
                record_capture_loss(
                    Some(Arc::clone(&sink)),
                    backend,
                    &entry,
                    "persist_failed",
                    &health,
                );
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
    health: CaptureHealth,
) -> bool {
    global().try_record(backend, sink, entry, health)
}

/// Next position in this process's capture order.
///
/// Monotonic and gap-free within the process, which is all the session read
/// needs: it is a tie-break inside one `created_at` tick, and a restart always
/// advances the wall clock past the tick it was in.
fn next_capture_seq() -> i64 {
    static CAPTURE_SEQ: AtomicU64 = AtomicU64::new(0);
    // Saturating rather than wrapping: a wrap would sort later rows before
    // earlier ones, and 2^63 captures in one process is not reachable.
    CAPTURE_SEQ
        .fetch_add(1, Ordering::Relaxed)
        .min(i64::MAX as u64) as i64
}

impl crate::state::ServerState {
    pub(crate) fn enqueue_trajectory_entry(&self, mut entry: TrajectoryEntry) -> bool {
        let Some((backend, sink)) = self.trajectory_sink() else {
            return true;
        };
        // Stamped here rather than at each capture site: this is the single
        // point every captured entry passes through, and it is still on the
        // capturing thread, before the entry is handed to a persistence task
        // that may land in any order.
        entry.capture_seq = Some(next_capture_seq());
        // Single choke point for every capture site: a captured body is
        // scrubbed of secret-named fields and then bounded, so a new capture
        // site cannot forget either, a stalled drain cannot accumulate whole
        // request bodies in memory, and the truncation preview can never carry
        // a value the full body would have had redacted.
        if let Some(body) = entry.request_body.take() {
            let redacted = crate::storage::redact_secrets(body);
            entry.request_body = Some(crate::storage::bounded_request_body(redacted));
        }
        try_record(backend, sink, entry, self.capture_health.clone())
    }
}

#[cfg(test)]
#[path = "trajectory_outbox_test.rs"]
mod tests;
