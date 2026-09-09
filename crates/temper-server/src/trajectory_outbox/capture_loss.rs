//! Recording a captured entry that never reached storage.
//!
//! The outbox next door is bounded and its writes can fail. Either way an
//! action the kernel captured is gone, and a loss that leaves no trace lets a
//! run with holes in it pass a conformance check later. So every loss is
//! counted, and the session it belonged to gets a marker row the checker reads
//! as an evidence gap (`crate::conformance::CAPTURE_LOSS_ENTITY_TYPE`).
//!
//! # The count leads the write
//!
//! [`CaptureHealth`] is incremented **before** the marker write is scheduled and
//! decremented only once storage confirms it. That ordering is the whole
//! guarantee: a process killed, a runtime cancelled, or a simulation ended
//! between the loss and the write leaves the count standing, so the server
//! reads degraded rather than clean. Counting after the write would make every
//! interruption look like a healthy capture — the exact fail-open this
//! mechanism exists to close.
//!
//! Two other things can go wrong, and neither is allowed to be silent: the
//! dedupe set can fill up, in which case the marker is written without being
//! remembered rather than skipped; and the write can keep failing, in which
//! case it is retried and the count simply stays up.
//!
//! # Scheduling
//!
//! Markers go through one bounded queue drained by one long-lived worker, the
//! same shape `crate::ots_trajectory_outbox` uses. `temper-server` is
//! simulation-visible, so the capture path spawns nothing per loss and holds no
//! `HashMap`/`HashSet`: a detached task per marker gives a simulation a
//! different interleaving on every run, and a hashed set gives it a different
//! iteration order.

use std::collections::BTreeSet;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use opentelemetry::KeyValue;
use temper_runtime::scheduler::sim_now;
use tokio::sync::mpsc;

use super::{metrics, next_capture_seq, record_dropped};
use crate::conformance::{CAPTURE_LOSS_ACTION, CAPTURE_LOSS_ENTITY_TYPE};
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::storage::TrajectorySink;

/// Cap on sessions this process remembers having marked.
///
/// The set exists only to keep a burst of losses in one session from becoming
/// a burst of identical marker writes. Past the cap the marker is still
/// written — the session that lost a row is the one that needs it — and only
/// the memory of having written it is dropped, so a long-lived process cannot
/// grow the set without bound and no loss goes unmarked to buy that.
pub(super) const MAX_MARKED_SESSIONS: usize = 4_096;

/// In-flight marker writes the queue will hold.
///
/// Small on purpose: a marker is one row, the worker drains them in order, and
/// a queue that cannot accept one leaves the loss counted rather than dropping
/// it silently.
const MARKER_QUEUE_CAPACITY: usize = 1_024;

/// Attempts a marker write gets before the worker gives up on it.
const CAPTURE_MARKER_ATTEMPTS: u32 = 4;

/// First backoff between marker attempts; doubles each time.
const CAPTURE_MARKER_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Capture losses this server has not confirmed recorded.
///
/// Incremented when an entry is lost, decremented only when the marker naming
/// its session is durably stored. Non-zero therefore covers every state in
/// which a reader could be misled: the write is still in flight, it exhausted
/// its retries, the queue would not take it, or the process died before any of
/// that finished. In all of them some stored session is missing rows with
/// nothing to say so, which is why a conformance check reports it and cannot
/// pass.
///
/// Carried on [`crate::state::ServerState`] rather than in a global, so it
/// belongs to one server's capture path and a check can ask its own server
/// whether the record it is about to read can be trusted.
#[derive(Clone, Default)]
pub(crate) struct CaptureHealth {
    unconfirmed_losses: Arc<AtomicU64>,
}

impl CaptureHealth {
    /// Losses whose marker this server has not seen stored.
    #[allow(dead_code)] // False positive: read by api/trajectory_analysis.rs under the observe feature
    pub(crate) fn unconfirmed_losses(&self) -> u64 {
        self.unconfirmed_losses.load(Ordering::Relaxed)
    }

    /// Whether any loss is still unaccounted for.
    #[allow(dead_code)] // False positive: read by api/trajectory_analysis.rs under the observe feature
    pub(crate) fn is_degraded(&self) -> bool {
        self.unconfirmed_losses() > 0
    }

    /// Count a loss, before anything is attempted on its behalf.
    pub(super) fn record_unconfirmed_loss(&self) {
        self.unconfirmed_losses.fetch_add(1, Ordering::Relaxed);
    }

    /// Clear one loss, once its marker is durably stored.
    fn confirm_loss_recorded(&self) {
        // Saturating: a decrement that ran without a matching increment would
        // wrap to a permanently degraded server.
        let _ =
            self.unconfirmed_losses
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    Some(count.saturating_sub(1))
                });
    }
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

/// One session's marker, waiting for the worker.
pub(super) struct QueuedMarker {
    sink: Arc<dyn TrajectorySink>,
    tenant: String,
    session_id: String,
    reason: &'static str,
    health: CaptureHealth,
}

/// The queue every marker write goes through.
///
/// One sender, one worker, started once. Markers are written in the order they
/// were queued, so a simulation replaying the same losses sees the same writes
/// in the same order.
fn marker_queue() -> &'static mpsc::Sender<QueuedMarker> {
    static QUEUE: OnceLock<mpsc::Sender<QueuedMarker>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (sender, receiver) = mpsc::channel(MARKER_QUEUE_CAPACITY);
        // determinism-ok: one long-lived drain task for external observe
        // persistence, started once — the same shape as the OTS trajectory
        // outbox worker, and not a task per captured loss.
        tokio::spawn(run_marker_worker(receiver));
        sender
    })
}

/// Drain markers in queue order, one at a time.
async fn run_marker_worker(mut receiver: mpsc::Receiver<QueuedMarker>) {
    while let Some(queued) = receiver.recv().await {
        persist_capture_loss_marker(queued, CAPTURE_MARKER_RETRY_DELAY).await;
    }
}

/// Sessions this process has already written a capture-loss marker for.
///
/// One marker per session is the whole signal: it says this session's stored
/// record has holes. Writing another on every subsequent loss would put a
/// write storm on the backend that is already the reason entries are being
/// lost.
///
/// A `BTreeSet` rather than a hashed one: this crate is simulation-visible and
/// ordered collections are what keep a replay identical.
fn marked_sessions() -> &'static Mutex<BTreeSet<(String, String)>> {
    static MARKED: OnceLock<Mutex<BTreeSet<(String, String)>>> = OnceLock::new();
    MARKED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// What the remembered set says about writing this session's marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkerClaim {
    /// Not marked yet, and now remembered: write it.
    Claimed,
    /// Already marked, or a marker is in flight: writing another says nothing
    /// the first one did not.
    AlreadyMarked,
    /// The set is full, so this write cannot be remembered. Write it anyway —
    /// a duplicate marker is noise, an unmarked lossy session is a silent hole.
    Unremembered,
}

/// Claim the right to write this session's marker.
pub(super) fn claim_capture_loss_marker(tenant: &str, session_id: &str) -> MarkerClaim {
    let Ok(mut marked) = marked_sessions().lock() else {
        // The set is poisoned, so nothing can be deduplicated against it. Write
        // rather than skip: the point of the marker is that its absence is
        // indistinguishable from a clean run.
        return MarkerClaim::Unremembered;
    };
    claim_in(&mut marked, tenant, session_id)
}

/// The claim decision itself, over whichever set is being deduplicated against.
///
/// Split from the lock so the full-set behaviour can be exercised on a set of
/// its own rather than by filling the one every other capture path shares.
pub(super) fn claim_in(
    marked: &mut BTreeSet<(String, String)>,
    tenant: &str,
    session_id: &str,
) -> MarkerClaim {
    let key = (tenant.to_string(), session_id.to_string());
    if marked.contains(&key) {
        return MarkerClaim::AlreadyMarked;
    }
    if marked.len() >= MAX_MARKED_SESSIONS {
        tracing::warn!(
            marked_sessions = marked.len(),
            tenant,
            session_id,
            "capture-loss marker dedupe set is full; marking without remembering"
        );
        return MarkerClaim::Unremembered;
    }
    marked.insert(key);
    MarkerClaim::Claimed
}

/// Give the claim back so a later loss in the same session tries again.
pub(super) fn release_capture_loss_marker(tenant: &str, session_id: &str) {
    if let Ok(mut marked) = marked_sessions().lock() {
        marked.remove(&(tenant.to_string(), session_id.to_string()));
    }
}

/// Record that a captured entry never reached storage.
///
/// The loss is counted first — on the metric and on [`CaptureHealth`] — and the
/// marker naming its session is queued after. Nothing between those two points
/// can make the loss disappear: if the queue refuses it, if the worker never
/// runs it, if the process dies first, the count is already standing and the
/// server reads degraded.
pub(super) fn record_capture_loss(
    sink: Option<Arc<dyn TrajectorySink>>,
    backend: &'static str,
    lost: &TrajectoryEntry,
    reason: &'static str,
    health: &CaptureHealth,
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

    // An entry with no session was never part of a run a conformance check can
    // read — a session read would not have returned it — so no session's report
    // is wrong for its absence and there is nothing to mark.
    let Some(session_id) = lost
        .session_id
        .as_deref()
        .filter(|session| !session.is_empty())
    else {
        return;
    };
    // No sink means no storage at all, and a conformance check against this
    // server answers 503 rather than reading a short session.
    let Some(sink) = sink else {
        return;
    };
    if claim_capture_loss_marker(&lost.tenant, session_id) == MarkerClaim::AlreadyMarked {
        return;
    }

    health.record_unconfirmed_loss();
    let queued = QueuedMarker {
        sink,
        tenant: lost.tenant.clone(),
        session_id: session_id.to_string(),
        reason,
        health: health.clone(),
    };
    if let Err(error) = marker_queue().try_send(queued) {
        // The count stays up, which is the point: a marker that was never
        // queued is a loss nothing durable will report.
        record_capture_loss_marker(&lost.tenant, "unqueued");
        release_capture_loss_marker(&lost.tenant, session_id);
        tracing::error!(
            tenant = %lost.tenant,
            session_id,
            reason = %error,
            "could not queue a trajectory capture-loss marker; the loss stays unconfirmed"
        );
    }
}

/// Write one session's capture-loss marker, retrying until it lands.
///
/// The retry belongs here rather than to the next loss: a session that loses
/// one row and fails to mark it would otherwise stay silently incomplete until
/// something else in the same session happened to be lost, which for a finished
/// run is never.
///
/// The loss is cleared from [`CaptureHealth`] only on a confirmed write. Every
/// other ending — exhausted retries, a cancelled worker, a dead process —
/// leaves it counted.
pub(super) async fn persist_capture_loss_marker(queued: QueuedMarker, retry_delay: Duration) {
    let QueuedMarker {
        sink,
        tenant,
        session_id,
        reason,
        health,
    } = queued;
    let marker = capture_loss_marker(&tenant, &session_id, reason);
    let mut delay = retry_delay;
    let mut last_error = String::new();
    for attempt in 1..=CAPTURE_MARKER_ATTEMPTS {
        match sink.persist_trajectory_entry(&marker).await {
            Ok(()) => {
                record_capture_loss_marker(&tenant, "stored");
                health.confirm_loss_recorded();
                return;
            }
            Err(error) => {
                last_error = error.to_string();
                tracing::warn!(
                    error = %last_error,
                    tenant = %tenant,
                    session_id = %session_id,
                    attempt,
                    max_attempts = CAPTURE_MARKER_ATTEMPTS,
                    "failed to persist trajectory capture-loss marker; retrying"
                );
                if attempt < CAPTURE_MARKER_ATTEMPTS && !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                delay = delay.saturating_mul(2);
            }
        }
    }

    record_capture_loss_marker(&tenant, "failed");
    // The count is left standing, and the session is no longer remembered as
    // marked, so a later loss in it gets a fresh attempt.
    release_capture_loss_marker(&tenant, &session_id);
    tracing::error!(
        error = %last_error,
        tenant = %tenant,
        session_id = %session_id,
        attempts = CAPTURE_MARKER_ATTEMPTS,
        "trajectory capture-loss marker exhausted its retries; the loss stays unconfirmed"
    );
}

/// Build a queued marker without going through the shared queue.
#[cfg(test)]
pub(super) fn queued_marker_for_test(
    sink: Arc<dyn TrajectorySink>,
    tenant: &str,
    session_id: &str,
    health: CaptureHealth,
) -> QueuedMarker {
    QueuedMarker {
        sink,
        tenant: tenant.to_string(),
        session_id: session_id.to_string(),
        reason: "outbox_full",
        health,
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
