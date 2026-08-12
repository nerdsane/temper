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
        capture_seq: None,
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

/// A sink whose every write fails, standing in for a backend that is down.
struct FailingSink;

#[async_trait::async_trait]
impl crate::storage::TrajectorySink for FailingSink {
    async fn persist_trajectory_entry(&self, _entry: &TrajectoryEntry) -> Result<(), String> {
        Err("backend unavailable".to_string())
    }
}

#[test]
fn a_session_is_marked_once() {
    // A backlog drops entries in bursts. One marker says the session's record
    // has holes; a thousand say the same thing to a backend that is already
    // the reason they are being dropped.
    release_capture_loss_marker("tenant", "burst-session");

    assert_eq!(
        claim_capture_loss_marker("tenant", "burst-session"),
        MarkerClaim::Claimed
    );
    assert_eq!(
        claim_capture_loss_marker("tenant", "burst-session"),
        MarkerClaim::AlreadyMarked
    );
    release_capture_loss_marker("tenant", "burst-session");
    assert_eq!(
        claim_capture_loss_marker("tenant", "burst-session"),
        MarkerClaim::Claimed,
        "a marker that failed to store leaves the session unmarked, so the next loss retries"
    );
    release_capture_loss_marker("tenant", "burst-session");
}

#[test]
fn a_full_dedupe_set_still_marks_the_session() {
    // The overflow behaviour that matters: past the cap the marker is written
    // without being remembered. Skipping it instead would make a lossy session
    // indistinguishable from a clean one, which is the whole failure the
    // marker exists to prevent.
    // On a set of its own, so filling it cannot make every other capture path
    // in the process look full.
    let mut marked = std::collections::HashSet::new();
    for index in 0..MAX_MARKED_SESSIONS {
        assert_eq!(
            claim_in(&mut marked, "tenant", &format!("session-{index}")),
            MarkerClaim::Claimed
        );
    }

    assert_eq!(
        claim_in(&mut marked, "tenant", "one-too-many"),
        MarkerClaim::Unremembered,
        "past the cap the marker is still written, only the memory of it is dropped"
    );
    assert_eq!(
        claim_in(&mut marked, "tenant", "one-too-many"),
        MarkerClaim::Unremembered,
        "an unremembered session must not later read as already marked"
    );
    assert_eq!(marked.len(), MAX_MARKED_SESSIONS, "the set stays bounded");
}

#[tokio::test]
async fn a_marker_that_never_lands_is_counted_as_an_unrecorded_loss() {
    // The retry runs on its own rather than waiting for the next loss — for a
    // finished run there is no next loss — and when it is exhausted the loss
    // is counted, because nothing durable will tell a reader this session has
    // a hole in it.
    release_capture_loss_marker("tenant", "doomed-session");
    let health = CaptureHealth::default();
    assert_eq!(health.unrecorded_losses(), 0);

    persist_capture_loss_marker(
        Arc::new(FailingSink),
        "tenant".to_string(),
        "doomed-session".to_string(),
        "persist_failed",
        health.clone(),
        Duration::ZERO,
    )
    .await;

    assert_eq!(
        health.unrecorded_losses(),
        1,
        "an unmarkable loss has to be visible somewhere, and storage is not available"
    );
    assert_eq!(
        claim_capture_loss_marker("tenant", "doomed-session"),
        MarkerClaim::Claimed,
        "the exhausted session is released so a later loss in it tries again"
    );
    release_capture_loss_marker("tenant", "doomed-session");
}

#[tokio::test]
async fn a_marker_that_lands_leaves_the_capture_healthy() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_url = format!("file:{}", dir.path().join("marker-ok.db").display());
    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso store");
    let health = CaptureHealth::default();

    persist_capture_loss_marker(
        Arc::new(store),
        "tenant".to_string(),
        "healthy-session".to_string(),
        "outbox_full",
        health.clone(),
        Duration::ZERO,
    )
    .await;

    assert_eq!(
        health.unrecorded_losses(),
        0,
        "a marker that stored is the loss being recorded, not an unrecorded one"
    );
}

#[test]
fn a_degraded_capture_stops_any_session_from_passing() {
    // A marker says which session lost a row. The degraded count says a row
    // was lost and could not be marked at all, so no session read from this
    // server can be assumed whole — including one that looks clean.
    let automaton = temper_spec::automaton::parse_automaton(include_str!(
        "../../../test-fixtures/specs/order.ioa.toml"
    ))
    .expect("order fixture parses");
    let rows = vec![temper_store_turso::TursoTrajectoryRow {
        tenant: "tenant".to_string(),
        entity_type: "Order".to_string(),
        entity_id: "order-1".to_string(),
        action: "AddItem".to_string(),
        success: true,
        from_status: Some("Draft".to_string()),
        to_status: Some("Draft".to_string()),
        error: None,
        agent_id: None,
        session_id: Some("session".to_string()),
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some("Entity".to_string()),
        spec_governed: Some(true),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        request_body: None,
        intent: None,
        matched_policy_ids: None,
        capture_seq: None,
    }];

    let input = |capture_degraded| crate::conformance::ConformanceInput {
        automaton: &automaton,
        kernel_rows: &rows,
        ots_trajectory: None,
        rows_truncated: false,
        spec_resolution: crate::conformance::SpecResolution::Pinned,
        capture_degraded,
    };

    assert!(
        crate::conformance::check_conformance(input(false)).passed,
        "the same run passes when the capture path is healthy"
    );
    let degraded = crate::conformance::check_conformance(input(true));
    assert!(!degraded.passed);
    assert!(!degraded.evidence_complete);
    assert!(
        degraded
            .evidence_gaps
            .iter()
            .any(|gap| gap.contains("could not record against any session")),
        "{:?}",
        degraded.evidence_gaps
    );
}

#[tokio::test]
async fn a_lost_entry_leaves_a_marker_the_checker_reads_as_missing_evidence() {
    // The end the finding cares about: after a loss, a conformance check
    // of that session must not come back `passed`.
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_url = format!("file:{}", dir.path().join("capture-loss.db").display());
    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso store");
    let session = "capture-loss-session";

    crate::storage::TrajectorySink::persist_trajectory_entry(
        &store,
        &TrajectoryEntry {
            session_id: Some(session.to_string()),
            ..entry("order-1")
        },
    )
    .await
    .expect("persist the entry that survived");

    persist_capture_loss_marker(
        Arc::new(store.clone()),
        "tenant".to_string(),
        session.to_string(),
        "outbox_full",
        CaptureHealth::default(),
        Duration::ZERO,
    )
    .await;
    release_capture_loss_marker("tenant", session);

    let rows = store
        .query_trajectories_by_session(session, Some("tenant"), None, 10)
        .await
        .expect("read the session back");
    assert_eq!(rows.len(), 2, "the marker is stored alongside the run");

    let automaton = temper_spec::automaton::parse_automaton(include_str!(
        "../../../test-fixtures/specs/order.ioa.toml"
    ))
    .expect("order fixture parses");
    let report = crate::conformance::check_conformance(crate::conformance::ConformanceInput {
        automaton: &automaton,
        kernel_rows: &rows,
        ots_trajectory: None,
        rows_truncated: false,
        spec_resolution: crate::conformance::SpecResolution::Pinned,
        capture_degraded: false,
    });

    assert_eq!(report.stats.capture_loss_markers, 1);
    assert!(
        !report.passed,
        "a run whose capture is known to have lost rows cannot pass: {:?}",
        report.evidence_gaps
    );
    assert!(!report.evidence_complete);
}
