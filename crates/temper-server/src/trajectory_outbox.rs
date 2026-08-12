//! Bounded background persistence for observe trajectory entries.
//!
//! The queue is bounded and its writes can fail, so this path can lose a
//! captured entry. A loss that only shows up as a log line lets a run with
//! holes in it later pass a conformance check, so every loss is counted and —
//! once per session — written to storage as a marker row the checker reads as
//! an evidence gap (`crate::conformance::CAPTURE_LOSS_ENTITY_TYPE`).

use std::collections::HashSet;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram},
};
use temper_runtime::scheduler::sim_now;
use tracing::Instrument;

use crate::conformance::{CAPTURE_LOSS_ACTION, CAPTURE_LOSS_ENTITY_TYPE};
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::storage::TrajectorySink;

const DEFAULT_CAPACITY: usize = 8_192;

/// Cap on sessions this process remembers having marked.
///
/// Past the cap the counter still records every loss; only the durable marker
/// is skipped, so a long-lived process cannot grow the set without bound.
const MAX_MARKED_SESSIONS: usize = 4_096;

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

fn record_capture_loss_marker(tenant: &str, result: &'static str) {
    metrics().capture_loss_marker_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("result", result),
        ],
    );
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
            record_capture_loss(sink.clone(), backend, &metric_entry, "outbox_full");
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
                // A write that failed loses the entry exactly as surely as a
                // full queue does. Both go through one place, so neither can
                // be the one that stays silent.
                record_capture_loss(Some(Arc::clone(&sink)), backend, &entry, "persist_failed");
            }
        }
    }
    .instrument(span)
    .await;
}

/// Sessions this process has already written a capture-loss marker for.
///
/// One marker per session is the whole signal: it says this session's stored
/// record has holes. Writing another on every subsequent loss would put a
/// write storm on the backend that is already the reason entries are being
/// lost.
fn marked_sessions() -> &'static Mutex<HashSet<(String, String)>> {
    static MARKED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    MARKED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Claim the right to write this session's one marker.
///
/// Returns false when a marker is already written or in flight, or when the
/// remembered set is full.
fn claim_capture_loss_marker(tenant: &str, session_id: &str) -> bool {
    let Ok(mut marked) = marked_sessions().lock() else {
        return false;
    };
    if marked.len() >= MAX_MARKED_SESSIONS {
        return false;
    }
    marked.insert((tenant.to_string(), session_id.to_string()))
}

/// Give the claim back so a later loss in the same session tries again.
fn release_capture_loss_marker(tenant: &str, session_id: &str) {
    if let Ok(mut marked) = marked_sessions().lock() {
        marked.remove(&(tenant.to_string(), session_id.to_string()));
    }
}

/// Record that a captured entry never reached storage.
///
/// Two things happen. The loss is counted, tagged with the reason, so the rate
/// is visible without reading logs. And the first time a session loses an
/// entry, a marker row is written for it, so a conformance check that reads
/// the session later learns the record it is walking is incomplete instead of
/// passing a run it only partly saw.
///
/// The marker goes straight to the sink rather than back through this outbox:
/// the outbox is what just failed, and a marker it drops says nothing.
fn record_capture_loss(
    sink: Option<Arc<dyn TrajectorySink>>,
    backend: &'static str,
    lost: &TrajectoryEntry,
    reason: &'static str,
) {
    record_dropped(lost, backend, reason);
    tracing::warn!(
        tenant = %lost.tenant,
        entity_type = %lost.entity_type,
        entity_id = %lost.entity_id,
        action = %lost.action,
        session_id = lost.session_id.as_deref().unwrap_or(""),
        reason,
        "trajectory capture lost an entry"
    );

    // An entry with no session belongs to no run a conformance check can read,
    // so there is no session record to mark; the counter carries the loss.
    let Some(session_id) = lost
        .session_id
        .as_deref()
        .filter(|session| !session.is_empty())
    else {
        return;
    };
    let Some(sink) = sink else {
        return;
    };
    if !claim_capture_loss_marker(&lost.tenant, session_id) {
        return;
    }

    // Off the caller's path: a loss is already a degraded state and the caller
    // is not waiting on the marker.
    tokio::spawn(persist_capture_loss_marker(
        sink,
        lost.tenant.clone(),
        session_id.to_string(),
        reason,
    ));
}

/// Write this session's capture-loss marker.
async fn persist_capture_loss_marker(
    sink: Arc<dyn TrajectorySink>,
    tenant: String,
    session_id: String,
    reason: &'static str,
) {
    let marker = capture_loss_marker(&tenant, &session_id, reason);
    match sink.persist_trajectory_entry(&marker).await {
        Ok(()) => record_capture_loss_marker(&tenant, "stored"),
        Err(error) => {
            record_capture_loss_marker(&tenant, "failed");
            // Nothing durable now says this session lost an entry, so let the
            // next loss in it try again rather than staying silent.
            release_capture_loss_marker(&tenant, &session_id);
            tracing::error!(
                error = %error,
                tenant = %tenant,
                session_id = %session_id,
                "failed to persist trajectory capture-loss marker"
            );
        }
    }
}

/// The row that tells a later reader this session's record is incomplete.
///
/// Not an actor's entity type and not a declared action, so the conformance
/// checker counts it as an evidence gap rather than judging it
/// (`crate::conformance::walk::row_disposition`).
fn capture_loss_marker(tenant: &str, session_id: &str, reason: &str) -> TrajectoryEntry {
    TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: CAPTURE_LOSS_ENTITY_TYPE.to_string(),
        entity_id: session_id.to_string(),
        action: CAPTURE_LOSS_ACTION.to_string(),
        success: false,
        from_status: None,
        to_status: None,
        error: Some(format!(
            "trajectory capture lost at least one entry for this session ({reason}); the stored \
             record of this run is incomplete"
        )),
        agent_id: None,
        session_id: Some(session_id.to_string()),
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Platform),
        spec_governed: Some(false),
        agent_type: None,
        request_body: None,
        intent: None,
        matched_policy_ids: None,
        capture_seq: Some(next_capture_seq()),
    }
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
        try_record(backend, sink, entry)
    }
}

#[cfg(test)]
#[path = "trajectory_outbox_test.rs"]
mod tests;
