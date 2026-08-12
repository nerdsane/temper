//! Recording a captured entry that never reached storage.
//!
//! The outbox next door is bounded and its writes can fail. Either way an
//! action the kernel captured is gone, and a loss that leaves no trace lets a
//! run with holes in it pass a conformance check later. So every loss is
//! counted, and the session it belonged to gets a marker row the checker reads
//! as an evidence gap (`crate::conformance::CAPTURE_LOSS_ENTITY_TYPE`).
//!
//! Two things can still go wrong, and neither is allowed to be silent: the
//! dedupe set can fill up, in which case the marker is written without being
//! remembered rather than skipped; and the marker write itself can fail, in
//! which case it is retried on its own schedule and, if it never lands, counted
//! on [`CaptureHealth`] — which every conformance check on this server reports.

use std::collections::HashSet;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use opentelemetry::KeyValue;
use temper_runtime::scheduler::sim_now;

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

/// Attempts a marker write gets before the loss counts as unrecordable.
const CAPTURE_MARKER_ATTEMPTS: u32 = 4;

/// First backoff between marker attempts; doubles each time.
const CAPTURE_MARKER_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Capture losses this process could not record anywhere a reader will find.
///
/// A marker write that exhausted its retries leaves nothing durable saying the
/// session's record has a hole in it, so the hole becomes invisible to the
/// conformance checker — exactly the failure the marker exists to prevent. The
/// count is carried on [`crate::state::ServerState`] rather than in a global,
/// so it belongs to one server's capture path and a conformance check can ask
/// its own server whether the record it is about to read can be trusted.
#[derive(Clone, Default)]
pub(crate) struct CaptureHealth {
    unrecorded_losses: Arc<AtomicU64>,
}

impl CaptureHealth {
    /// Losses this process failed to record against any session.
    ///
    /// Non-zero means some stored session is incomplete and nothing says
    /// which, so every conformance check has to report it.
    pub(crate) fn unrecorded_losses(&self) -> u64 {
        self.unrecorded_losses.load(Ordering::Relaxed)
    }

    fn record_unrecorded_loss(&self) {
        self.unrecorded_losses.fetch_add(1, Ordering::Relaxed);
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
    marked: &mut HashSet<(String, String)>,
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
/// Two things happen. The loss is counted, tagged with the reason, so the rate
/// is visible without reading logs. And the first time a session loses an
/// entry, a marker row is written for it, so a conformance check that reads
/// the session later learns the record it is walking is incomplete instead of
/// passing a run it only partly saw.
///
/// The marker goes straight to the sink rather than back through this outbox:
/// the outbox is what just failed, and a marker it drops says nothing.
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
    if claim_capture_loss_marker(&lost.tenant, session_id) == MarkerClaim::AlreadyMarked {
        return;
    }

    // Off the caller's path: a loss is already a degraded state and the caller
    // is not waiting on the marker.
    tokio::spawn(persist_capture_loss_marker(
        sink,
        lost.tenant.clone(),
        session_id.to_string(),
        reason,
        health.clone(),
        CAPTURE_MARKER_RETRY_DELAY,
    ));
}

/// Write this session's capture-loss marker, retrying until it lands.
///
/// The retry loop belongs here rather than to the next loss: a session that
/// loses one row and fails to mark it would otherwise stay silently incomplete
/// until something else in the same session happened to be lost, which for a
/// finished run is never.
///
/// When every attempt fails the loss is unrecordable — nothing durable will
/// tell a later reader this session has a hole — so it is counted on
/// [`CaptureHealth`], which every conformance check on this server reports.
pub(super) async fn persist_capture_loss_marker(
    sink: Arc<dyn TrajectorySink>,
    tenant: String,
    session_id: String,
    reason: &'static str,
    health: CaptureHealth,
    retry_delay: Duration,
) {
    let marker = capture_loss_marker(&tenant, &session_id, reason);
    let mut delay = retry_delay;
    let mut last_error = String::new();
    for attempt in 1..=CAPTURE_MARKER_ATTEMPTS {
        match sink.persist_trajectory_entry(&marker).await {
            Ok(()) => {
                record_capture_loss_marker(&tenant, "stored");
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
    health.record_unrecorded_loss();
    // The session is no longer remembered as marked, so a later loss in it
    // gets a fresh attempt on top of the count above.
    release_capture_loss_marker(&tenant, &session_id);
    tracing::error!(
        error = %last_error,
        tenant = %tenant,
        session_id = %session_id,
        attempts = CAPTURE_MARKER_ATTEMPTS,
        "trajectory capture-loss marker exhausted its retries; the loss is unrecorded"
    );
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
