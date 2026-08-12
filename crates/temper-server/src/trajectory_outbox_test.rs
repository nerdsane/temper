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

#[test]
fn a_session_is_marked_once() {
    // A backlog drops entries in bursts. One marker says the session's
    // record has holes; a thousand say the same thing to a backend that is
    // already the reason they are being dropped.
    assert!(claim_capture_loss_marker("tenant", "burst-session"));
    assert!(!claim_capture_loss_marker("tenant", "burst-session"));
    release_capture_loss_marker("tenant", "burst-session");
    assert!(
        claim_capture_loss_marker("tenant", "burst-session"),
        "a marker that failed to store leaves the session unmarked, so the next loss retries"
    );
    release_capture_loss_marker("tenant", "burst-session");
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
    });

    assert_eq!(report.stats.capture_loss_markers, 1);
    assert!(
        !report.passed,
        "a run whose capture is known to have lost rows cannot pass: {:?}",
        report.evidence_gaps
    );
    assert!(!report.evidence_complete);
}
