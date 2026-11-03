//! The background trim keeps only what the remote has not confirmed in the
//! local change log, and change numbers keep rising, so no change is skipped.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use temper_store_turso::TursoEventStore;

use super::support::*;

/// What the background loop gives a trim each tick (temper-cli's `TRIM_BUDGET`).
const TRIM_BUDGET: Duration = Duration::from_secs(1);

async fn trim(store: &TursoEventStore) -> u64 {
    store.trim_change_log(TRIM_BUDGET).await.expect("trim")
}

#[tokio::test]
async fn pushed_changes_leave_the_change_log() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let a = server.open(&path).await;
    let copy = a.sync_copy().expect("copy").clone();
    let mut newest = copy.last_change().await.expect("newest change");
    for n in 0..30 {
        write(&a, "c1", n).await;
        push(&a).await;
        let now = copy.last_change().await.expect("newest change");
        assert!(now > newest, "change numbers rise: {now} after {newest}");
        newest = now;
    }

    trim(&a).await;
    let rows = copy.change_log_rows().await.expect("change log rows");
    assert!(
        rows <= 5,
        "30 pushed writes leave {rows} change log rows; the remote has them all"
    );
    assert_eq!(
        copy.last_change().await.expect("newest change"),
        newest,
        "the trim keeps the newest change"
    );

    // What is not pushed yet stays.
    write(&a, "c1", 30).await;
    let unpushed = copy.change_log_bytes_after(newest).await.expect("bytes");
    assert!(unpushed > 0);
    trim(&a).await;
    assert_eq!(
        copy.change_log_bytes_after(newest).await.expect("bytes"),
        unpushed
    );
    push(&a).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "c1").await, 31);
}

#[tokio::test]
async fn no_change_is_skipped_across_a_trim_a_restart_and_a_failed_push() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, on) = switch(&server.url).await;
    let open = |path: std::path::PathBuf| {
        let url = url.clone();
        async move {
            TursoEventStore::new_synced(path.to_str().expect("utf-8"), &url, TOKEN)
                .await
                .expect("open")
        }
    };
    let (_a_dir, a_path) = machine();
    let a = open(a_path.clone()).await;
    let first = a.sync_copy().expect("copy").clone();
    for n in 0..5 {
        write(&a, "k1", n).await;
        push(&a).await;
        trim(&a).await;
    }
    let before_restart = first.last_change().await.expect("newest change");
    drop(first);
    drop(a);

    let a = open(a_path.clone()).await;
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(
        copy.last_change().await.expect("newest change"),
        before_restart,
        "a restart finds the newest change number where the last run left it"
    );
    write(&a, "k1", 5).await;
    assert!(copy.last_change().await.expect("newest change") > before_restart);

    // A push that fails confirms nothing, so the trim keeps what it carried.
    on.store(false, Ordering::Release);
    copy.push_committed()
        .await
        .expect_err("the remote cannot be reached");
    on.store(true, Ordering::Release);
    let unpushed = copy
        .change_log_bytes_after(before_restart)
        .await
        .expect("bytes");
    assert!(unpushed > 0);
    trim(&a).await;
    assert_eq!(
        copy.change_log_bytes_after(before_restart)
            .await
            .expect("bytes"),
        unpushed
    );
    write(&a, "k1", 6).await;
    copy.push_committed_within(std::time::Duration::from_secs(30))
        .await
        .expect("push once the remote is back");
    trim(&a).await;

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "k1").await, 7, "every change reached the remote");
}

#[tokio::test]
async fn the_background_trim_clears_a_long_confirmed_log_in_budgeted_calls() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let (a, copy) = confirmed_log(&server, &path, BIG_LOG).await;
    let mut newest = copy.last_change().await.expect("newest change");
    let mut rows = copy.change_log_rows().await.expect("rows");
    let mut written = 0;
    let mut calls = 0;
    while rows > 5 {
        assert!(
            calls < 60,
            "{rows} change log rows are left after {calls} trims"
        );
        // Writes keep landing between trims, as on a live app.
        write(&a, "live", written).await;
        written += 1;
        push(&a).await;
        let pushed = copy.last_change().await.expect("newest change");
        assert!(
            pushed > newest,
            "change numbers rise: {pushed} after {newest}"
        );
        newest = pushed;

        let started = Instant::now();
        trim(&a).await;
        let took = started.elapsed();
        assert!(
            took < TRIM_BUDGET * 3,
            "a trim given {TRIM_BUDGET:?} took {took:?}"
        );
        copy.checkpoint_if_wal_over(16 * 1024 * 1024)
            .await
            .expect("checkpoint");
        assert_eq!(
            copy.last_change().await.expect("newest change"),
            newest,
            "the trim keeps the newest change"
        );
        rows = copy.change_log_rows().await.expect("rows");
        calls += 1;
    }

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(
        count(&b, "live").await,
        written as usize,
        "every write made while trimming reached the remote"
    );
    assert_eq!(count(&b, "pushed").await, 1);
}

/// Writes between two trims: with the trims a second apart, about the
/// Factory's growth of 56 change log rows a second.
const WRITES_PER_TICK: u64 = 20;

/// Trims in the run: enough for the batches to reach their size.
const TICKS: u64 = 8;

#[tokio::test]
async fn the_trim_outpaces_the_log_while_every_commit_is_slow() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let (a, copy) = confirmed_log(&server, &path, 20_000).await;
    // One commit on a Tensorlake drive, whatever its rows (roll3, staging
    // canary, 26 Oct 2026).
    copy.simulate_slow_trim(Duration::ZERO, Duration::from_millis(170));
    let rows_before = copy.change_log_rows().await.expect("rows");
    let mut added = 0;
    let mut trimmed = 0;
    let started = Instant::now();
    for tick in 0..TICKS {
        // Writes keep landing between trims, as on a live app.
        let newest = copy.last_change().await.expect("newest change");
        for n in 0..WRITES_PER_TICK {
            write(&a, &format!("tick{tick}"), n).await;
        }
        push(&a).await;
        added += copy.last_change().await.expect("newest change") - newest;
        trimmed += trim(&a).await;
    }
    let rows_after = copy.change_log_rows().await.expect("rows");
    let seconds = started.elapsed().as_secs_f64();
    eprintln!(
        "{trimmed} rows trimmed and {added} added in {seconds:.1} s; \
         the log went from {rows_before} to {rows_after} rows"
    );
    assert!(
        trimmed > u64::try_from(added).expect("rows"),
        "the trim deleted {trimmed} rows while writes added {added}"
    );
    assert!(
        rows_after < rows_before,
        "the log grew from {rows_before} to {rows_after} rows while writes landed"
    );
}
