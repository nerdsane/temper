//! A refused composite sub-write fails the composite and runs its `on_failure`.
//!
//! A Notice's `PutItLive` is a composite whose wasm trigger answers with one
//! sub-write: the Release's `Approve`. When that sub-write is refused, either
//! by Cedar or because the Release has already moved on and its guard no longer
//! allows `Approve`, none of the composite's writes land, and the Notice's
//! declared `on_failure` (`Missed`) runs with the refusal as its `error`.
//! Before, the refusal left the Notice in `Answered` as if the answer had moved
//! the Release. Each case runs with an event journal (the atomic batch) and
//! without one (the per-write path). A sub-write that commits is not a
//! refusal, even when a later step of its write reports a failure. Neither is
//! one whose storage fails, since that write may have landed, nor one the store
//! wrote although it answered with a concurrency violation. A sub-write that
//! meets a journal another server moved, and no longer applies once the
//! journal is read again, is a refusal.

#[path = "composite_refusal/support.rs"]
mod support;

use std::time::Duration;

use temper_runtime::persistence::EventStore;
use temper_server::StorageStack;
use temper_store_sim::SimEventStore;

use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_moves_the_release_when_nothing_refuses_it() {
    for journal in [Journal::Atomic, Journal::PerWrite] {
        let state = notice_state(journal, PERMIT_ALL);
        act(&state, "Release", RELEASE, "Hold").await;

        act(&state, "Notice", NOTICE, "PutItLive").await;

        let (release, _) = settle(&state, "Release", RELEASE, &["Live"]).await;
        assert_eq!(release, "Live", "{journal:?}: the sub-write should apply");
        assert_eq!(
            status_of(&state, "Notice", NOTICE).await,
            "Answered",
            "{journal:?}: an applied composite does not run on_failure"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_write_refused_by_its_guard_runs_the_composites_on_failure() {
    for journal in [Journal::Atomic, Journal::PerWrite] {
        let state = notice_state(journal, PERMIT_ALL);
        act(&state, "Release", RELEASE, "Hold").await;
        // The Release moves on before the owner answers, so `Approve` no
        // longer applies from its state.
        act(&state, "Release", RELEASE, "Reject").await;

        act(&state, "Notice", NOTICE, "PutItLive").await;

        let (notice, fields) = settle(&state, "Notice", NOTICE, &["Closed"]).await;
        assert_eq!(
            notice, "Closed",
            "{journal:?}: the refused sub-write should run Missed"
        );
        let error = error_field(&fields);
        assert!(
            error.contains("sub-write 0") && error.contains("Approve"),
            "{journal:?}: Missed should carry the refusal, got {error:?}"
        );
        assert_eq!(
            status_of(&state, "Release", RELEASE).await,
            "Out",
            "{journal:?}: the refused sub-write leaves the Release as it was"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_write_refused_by_cedar_runs_the_composites_on_failure() {
    for journal in [Journal::Atomic, Journal::PerWrite] {
        let state = notice_state(journal, FORBID_APPROVE);
        act(&state, "Release", RELEASE, "Hold").await;

        act(&state, "Notice", NOTICE, "PutItLive").await;

        let (notice, fields) = settle(&state, "Notice", NOTICE, &["Closed"]).await;
        assert_eq!(
            notice, "Closed",
            "{journal:?}: the denied sub-write should run Missed"
        );
        let error = error_field(&fields);
        assert!(
            error.contains("denied") && error.contains("Release.Approve"),
            "{journal:?}: Missed should carry the denial, got {error:?}"
        );
        assert_eq!(
            status_of(&state, "Release", RELEASE).await,
            "Held",
            "{journal:?}: the denied sub-write leaves the Release as it was"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_write_whose_storage_fails_does_not_run_the_composites_on_failure() {
    for (path, release_ioa) in [
        ("atomic batch", RELEASE_IOA.to_string()),
        ("per-write", release_with_own_effect()),
    ] {
        let store = SimEventStore::no_faults(37);
        let stack = StorageStack::from_sim(store.clone(), None);
        let state = notice_server(&release_ioa, Some(stack), PERMIT_ALL);
        act(&state, "Release", RELEASE, "Hold").await;
        const FAILURES: usize = 100;
        store.fail_next_appends(RELEASE_JOURNAL, FAILURES);

        act(&state, "Notice", NOTICE, "PutItLive").await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while store.pending_append_failures(RELEASE_JOURNAL) == FAILURES
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            store.pending_append_failures(RELEASE_JOURNAL) < FAILURES,
            "{path}: the composite should have tried to write the Release"
        );
        notice_stays_answered(
            &state,
            &format!("{path}: a sub-write whose storage failed does not run on_failure"),
        )
        .await;
        assert_eq!(
            status_of(&state, "Release", RELEASE).await,
            "Held",
            "{path}: the failed write leaves the Release as it was"
        );
    }
}

/// Another server sharing the event store rejects the Release while this one
/// still holds it as `Held`, and this server's `Approve` meets the moved
/// journal. The atomic batch is refused as a conflict. On the per-write path
/// the actor reads the journal again, finds the Release `Out` and no
/// `Approve`, and its guard refuses the sub-write. Either way `Missed` runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_sub_write_refused_after_another_server_moved_the_release_runs_on_failure() {
    for (path, release_ioa, refusal) in [
        ("atomic batch", RELEASE_IOA.to_string(), "conflict"),
        (
            "per-write",
            release_with_own_effect(),
            "Action 'Approve' not valid from state 'Out'",
        ),
    ] {
        let store = SimEventStore::no_faults(41);
        let server = |store: &SimEventStore| {
            let stack = StorageStack::from_sim(store.clone(), None);
            notice_server(&release_ioa, Some(stack), PERMIT_ALL)
        };
        let here = server(&store);
        let elsewhere = server(&store);
        act(&here, "Release", RELEASE, "Hold").await;
        act(&elsewhere, "Release", RELEASE, "Reject").await;
        assert_eq!(
            status_of(&here, "Release", RELEASE).await,
            "Held",
            "{path}: this server has not read the Reject yet"
        );

        act(&here, "Notice", NOTICE, "PutItLive").await;

        let (notice, fields) = settle(&here, "Notice", NOTICE, &["Closed"]).await;
        assert_eq!(
            notice, "Closed",
            "{path}: the refused sub-write should run Missed"
        );
        let error = error_field(&fields);
        assert!(
            error.contains(refusal),
            "{path}: Missed should carry the refusal {refusal:?}, got {error:?}"
        );
        assert!(
            !error.contains("would fail"),
            "{path}: the refusal should come from the write, not the composite's preflight, got {error:?}"
        );
    }
}

/// The store writes the Release's `Approve` and then loses its answer, so it
/// reports a concurrency violation, as the Turso store does when it retries a
/// write whose first answer was lost. Read again, the journal holds the
/// `Approve`: the sub-write landed, so it is not a refusal, `on_failure` does
/// not run, and the Release is approved once. With the atomic batch the whole
/// batch landed behind the violation.
#[tokio::test(flavor = "multi_thread")]
async fn a_sub_write_that_landed_behind_a_concurrency_violation_does_not_run_on_failure() {
    for (path, release_ioa) in [
        ("atomic batch", RELEASE_IOA.to_string()),
        ("per-write", release_with_own_effect()),
    ] {
        let store = SimEventStore::no_faults(43);
        let stack = StorageStack::from_sim(store.clone(), None);
        let state = notice_server(&release_ioa, Some(stack), PERMIT_ALL);
        act(&state, "Release", RELEASE, "Hold").await;
        store.lose_next_append_answers(RELEASE_JOURNAL, 1);

        act(&state, "Notice", NOTICE, "PutItLive").await;

        let (release, _) = settle(&state, "Release", RELEASE, &["Live"]).await;
        assert_eq!(release, "Live", "{path}: the Approve landed");
        assert_eq!(
            store.pending_lost_append_answers(RELEASE_JOURNAL),
            0,
            "{path}: the Approve's answer was lost"
        );
        notice_stays_answered(
            &state,
            &format!("{path}: a sub-write that landed does not run on_failure"),
        )
        .await;
        let approvals = store
            .read_events(RELEASE_JOURNAL, 0)
            .await
            .expect("read the Release journal")
            .into_iter()
            .filter(|event| event.event_type == "Approve")
            .count();
        assert_eq!(approvals, 1, "{path}: the Approve is written once");
    }
}
