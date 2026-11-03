//! An answer waits only for its own push, never for the change log's trim,
//! however long the confirmed part of the log has grown, and its writes wait
//! for one trim batch at most.

use std::time::{Duration, Instant};

use super::support::*;

/// The time an answer has for its push (temper-cli's `ANSWER_BUDGET`).
const ANSWER_BUDGET: Duration = Duration::from_secs(10);

#[tokio::test]
async fn no_answer_waits_on_a_trim() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let (a, copy) = confirmed_log(&server, &path, BIG_LOG).await;

    write(&a, "answered", 0).await;
    let rows = copy.change_log_rows().await.expect("rows");
    let started = Instant::now();
    copy.push_committed_within(ANSWER_BUDGET)
        .await
        .expect("the answer's push");
    let took = started.elapsed();

    let left = copy.change_log_rows().await.expect("rows");
    assert_eq!(
        left,
        rows,
        "an answer's push deleted {} confirmed change log rows",
        rows - left
    );
    assert!(took < ANSWER_BUDGET, "an answer's push took {took:?}");

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(
        count(&b, "answered").await,
        1,
        "the answer's change reached the remote"
    );
}

/// A write's default wait for the database's write lock before its move is
/// refused (the store's append attempt timeout).
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// A write that waits while a trim batch holds the lock goes before the
/// trim's next batch, even on a drive whose commit takes seconds.
#[tokio::test]
async fn a_write_waiting_during_a_trim_batch_goes_before_the_next_batch() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    // Enough confirmed rows that the trim has many batches left to run.
    let (a, copy) = confirmed_log(&server, &path, 30_000).await;
    let commit = Duration::from_secs(2);
    copy.simulate_slow_trim(Duration::ZERO, commit);
    let first_batch = copy.next_trim_rows();
    let rows_before = copy.change_log_rows().await.expect("rows");
    let newest_before = copy.last_change().await.expect("newest change");

    let started = Instant::now();
    let trim = a.trim_change_log(Duration::from_secs(7));
    let waiting_write = async {
        // Arrive while the first batch commits.
        tokio::time::sleep(commit / 4).await;
        let arrived = started.elapsed();
        write(&a, "waiting", 0).await;
        let waited = started.elapsed() - arrived;
        // Count at once: a batch that began after the write is still in its
        // commit, so the rows it deletes are still here.
        let rows = copy.change_log_rows().await.expect("rows");
        let added = copy.last_change().await.expect("newest change") - newest_before;
        (waited, rows_before + added - rows)
    };
    let (trimmed, (waited, deleted)) = tokio::join!(trim, waiting_write);
    let trimmed = trimmed.expect("trim");
    eprintln!(
        "the write waited {} ms and landed after {deleted} trimmed rows; the call trimmed {trimmed}",
        waited.as_millis()
    );
    assert!(
        deleted <= first_batch,
        "{deleted} rows were trimmed before the waiting write landed, more than the \
         {first_batch} of the batch it arrived during"
    );
    assert!(
        waited < LOCK_WAIT,
        "the write waited {waited:?} for the trim, past its {LOCK_WAIT:?} wait for the lock"
    );
    assert!(
        trimmed > u64::try_from(deleted).expect("rows"),
        "the trim went on after the write"
    );
}

/// A batch whose rows turn out far slower than planned stops for a write
/// that waits, rather than holding the lock to its end.
#[tokio::test]
async fn a_trim_batch_that_overruns_its_plan_gives_way_to_a_waiting_write() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let (a, copy) = confirmed_log(&server, &path, 30_000).await;
    // Cold rows on a slow drive: the first batch's DELETE takes seconds.
    let work = Duration::from_secs(3);
    copy.simulate_slow_trim(work, Duration::ZERO);
    let rows_before = copy.change_log_rows().await.expect("rows");
    let newest_before = copy.last_change().await.expect("newest change");

    let started = Instant::now();
    let trim = a.trim_change_log(Duration::from_secs(1));
    let waiting_write = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let arrived = started.elapsed();
        write(&a, "waiting", 0).await;
        let waited = started.elapsed() - arrived;
        let rows = copy.change_log_rows().await.expect("rows");
        let added = copy.last_change().await.expect("newest change") - newest_before;
        (waited, rows_before + added - rows)
    };
    let (trimmed, (waited, deleted)) = tokio::join!(trim, waiting_write);
    let trimmed = trimmed.expect("trim");
    eprintln!(
        "the write waited {} ms; {deleted} rows were trimmed before it and {trimmed} in the call",
        waited.as_millis()
    );
    assert!(
        waited < work / 3,
        "the write waited {waited:?} behind a DELETE planned for far less"
    );
    assert_eq!(deleted, 0, "the stopped batch deleted nothing");
}
