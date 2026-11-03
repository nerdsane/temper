//! A synced store copies confirmed writes to its remote database, and a fresh
//! machine restores them from it.
//!
//! Runs against Turso's local sync server: set `TEMPER_TEST_TURSODB` to a
//! `tursodb` binary of the version the `turso` crate pins (0.7.2). Without it
//! the tests are skipped locally and fail in CI.

mod answers;
mod change_log;
mod hung_push;
mod projection;
mod startup;
mod support;

use std::time::{Duration, Instant};

use support::*;

use temper_store_turso::{CopyState, SyncCopy, TursoEventStore};

#[tokio::test]
async fn a_pushed_write_is_restored_on_a_fresh_machine_and_an_unpushed_one_is_not() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    write(&a, "n1", 0).await;
    push(&a).await;
    write(&a, "n1", 1).await;

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "n1").await, 1, "only the pushed write is remote");
    drop(b);

    push(&a).await;
    let (_c_dir, c_path) = machine();
    let c = server.open(&c_path).await;
    assert_eq!(count(&c, "n1").await, 2);
}

#[tokio::test]
async fn writes_that_commit_together_all_reach_the_remote() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = std::sync::Arc::new(server.open(&a_path).await);
    let writers: Vec<_> = (0..40)
        .map(|n| {
            let a = a.clone();
            tokio::spawn(async move {
                write(&a, &format!("e{n}"), 0).await;
                push(&a).await;
            })
        })
        .collect();
    for writer in writers {
        writer.await.expect("writer");
    }

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    for n in 0..40 {
        assert_eq!(count(&b, &format!("e{n}")).await, 1, "entity e{n}");
    }
}

#[tokio::test]
async fn a_restart_keeps_local_writes_and_sends_them() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    write(&a, "r1", 0).await;
    drop(a);

    let a = server.open(&a_path).await;
    assert_eq!(
        count(&a, "r1").await,
        1,
        "the restart did not replace the local file"
    );

    // The next push sends what the last run left behind; a server makes one
    // as soon as it serves.
    push(&a).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "r1").await, 1);
}

#[tokio::test]
async fn a_database_written_before_sync_is_refused_not_replaced() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let plain = TursoEventStore::new(&format!("file:{}", path.display()), None)
        .await
        .expect("plain store");
    write(&plain, "p1", 0).await;
    drop(plain);

    let error = TursoEventStore::new_synced(path.to_str().expect("utf-8"), &server.url, TOKEN)
        .await
        .expect_err("must refuse");
    assert!(error.to_string().contains("never copied"), "{error}");

    let plain = TursoEventStore::new(&format!("file:{}", path.display()), None)
        .await
        .expect("plain store");
    assert_eq!(count(&plain, "p1").await, 1, "the local file is untouched");
}

#[tokio::test]
async fn once_the_remote_refuses_a_change_no_later_push_succeeds() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    write(&a, "d1", 0).await;
    push(&a).await;

    // The remote now refuses the next event. It still commits the rest of
    // that push and its progress marker, so the engine alone would report the
    // retry as a success with the event missing.
    server.remote_sql("DROP TABLE events");
    write(&a, "d1", 1).await;
    let copy = a.sync_copy().expect("copy");
    copy.push_committed()
        .await
        .expect_err("the remote refused the event");
    let error = copy
        .push_committed()
        .await
        .expect_err("a retry must not confirm a copy that lost a change");
    assert!(error.to_string().contains("missing a change"), "{error}");
}

#[tokio::test]
async fn a_spec_and_a_module_rewritten_in_place_reach_the_remote() {
    // An upsert that updates an existing row replays on the remote as an
    // upsert only when the table has a primary key; with UNIQUE alone the
    // remote refused it as a duplicate.
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    for version in ["v1", "v2"] {
        a.upsert_specs_and_commit(
            "default",
            &[("Note", version, "<csdl/>", version)],
            None,
            "notes",
        )
        .await
        .expect("upsert spec");
        a.upsert_wasm_module("default", "notes", version.as_bytes(), version, "bundled")
            .await
            .expect("upsert module");
        push(&a).await;
    }

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    let specs = b.load_specs().await.expect("load specs");
    let note = specs
        .iter()
        .find(|spec| spec.entity_type == "Note")
        .expect("spec restored");
    assert_eq!(note.ioa_source, "v2");
    let module = b
        .load_wasm_module("default", "notes")
        .await
        .expect("load module")
        .expect("module restored");
    assert_eq!(module.sha256_hash, "v2");
}

#[tokio::test]
async fn a_table_without_a_primary_key_is_refused() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    drop(server.open(&a_path).await);
    server.remote_sql("CREATE TABLE loose (value TEXT)");

    let (_b_dir, b_path) = machine();
    let error = TursoEventStore::new_synced(b_path.to_str().expect("utf-8"), &server.url, TOKEN)
        .await
        .expect_err("must refuse");
    assert!(error.to_string().contains("loose"), "{error}");
}

#[tokio::test]
async fn checkpoints_between_writes_fail_none_of_them() {
    // While the engine checkpoints, any statement on any connection fails at
    // once with "database is locked"; a checkpoint must wait for a moment
    // when no connection is in use.
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = std::sync::Arc::new(server.open(&a_path).await);
    let writers: Vec<_> = (0..8)
        .map(|n| {
            let a = a.clone();
            tokio::spawn(async move {
                for i in 0..15 {
                    write(&a, &format!("c{n}"), i).await;
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                push(&a).await;
            })
        })
        .collect();
    let copy = a.sync_copy().expect("copy").clone();
    let mut checkpoints = 0;
    while writers.iter().any(|writer| !writer.is_finished()) {
        if copy.checkpoint_if_wal_over(0).await.expect("checkpoint") {
            checkpoints += 1;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    for writer in writers {
        writer.await.expect("writer");
    }
    assert!(
        checkpoints > 0,
        "no checkpoint ran while writes were going on"
    );

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    for n in 0..8 {
        assert_eq!(count(&b, &format!("c{n}")).await, 15, "entity c{n}");
    }
}

/// A checkpoint waits for the calls in flight instead of skipping, and says
/// how long it waited and how long it took: every call that needed a
/// connection meanwhile, the write holding the store's write gate included,
/// waited as long.
#[tokio::test]
async fn a_checkpoint_waits_for_the_calls_in_flight_instead_of_skipping() {
    // Skipping whenever a connection was in use let the log grow without
    // bound under steady writes (1.21 GB in four minutes, staging, 27 Oct
    // 2026), and the checkpoint that finally ran moved all of it while every
    // call waited.
    let Some(server) = SyncServer::start() else {
        return;
    };
    let lines = Lines::default();
    // This thread's own listener: the test runtime runs every task here.
    let _listening =
        tracing::subscriber::set_default(tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            lines.clone(),
        ));
    let (_a_dir, a_path) = machine();
    let a = std::sync::Arc::new(server.open(&a_path).await);
    for i in 0..10 {
        write(&a, "c1", i).await;
    }
    push(&a).await;
    let copy = a.sync_copy().expect("copy").clone();
    // A trim holds one connection through its batches until its budget runs
    // out. Given 100 ms, it starts one batch and lets go when that batch's
    // commit returns, about 400 ms in: a call in flight that ends within the
    // checkpoint's turn.
    copy.simulate_slow_trim(Duration::ZERO, Duration::from_millis(400));
    let trimming = {
        let a = a.clone();
        tokio::spawn(async move { a.trim_change_log(Duration::from_millis(100)).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !trimming.is_finished(),
        "the trim still holds its connection"
    );

    let asked = Instant::now();
    assert!(
        copy.checkpoint_if_wal_over(0).await.expect("checkpoint"),
        "the checkpoint ran once the trim let go of its connection"
    );
    let took = asked.elapsed();
    let held = lines
        .find(|line| line.message == "turso.sync checkpoint held new calls")
        .expect("a checkpoint that waited for the trim says so");
    let field = |name: &str| -> u128 {
        held.fields[name]
            .parse()
            .unwrap_or_else(|_| panic!("{name} in {held:?}"))
    };
    assert!(
        field("waited_ms") >= 100 && field("waited_ms") + field("fold_ms") <= took.as_millis(),
        "the line gives the wait for the trim's connection and the fold, \
         within the {} ms the call took: {held:?}",
        took.as_millis()
    );
    let trimmed = trimming.await.expect("trim task").expect("trim");
    assert!(trimmed > 0, "the trim deleted the pushed changes");
    write(&a, "c1", 10).await;
    push(&a).await;

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "c1").await, 11, "every write reached the remote");
}

#[tokio::test]
async fn a_file_put_back_from_an_older_copy_is_restored_from_the_remote() {
    // A restart can bring back an old snapshot of the disk. The engine would
    // then reuse change numbers the remote already has and skip the new
    // writes while reporting the push as done.
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (a_dir, a_path) = machine();
    let older = tempfile::tempdir().expect("snapshot dir");
    let a = server.open(&a_path).await;
    write(&a, "old", 0).await;
    push(&a).await;
    drop(a);
    copy_files(a_dir.path(), older.path());

    let a = server.open(&a_path).await;
    for i in 0..5 {
        write(&a, "later", i).await;
    }
    push(&a).await;
    drop(a);
    remove_files(a_dir.path());
    copy_files(older.path(), a_dir.path());

    let a = server.open(&a_path).await;
    assert_eq!(count(&a, "later").await, 5, "restored from the remote");
    write(&a, "after", 0).await;
    push(&a).await;

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "later").await, 5);
    assert_eq!(
        count(&b, "after").await,
        1,
        "the write after the restore is remote"
    );
}

#[tokio::test]
async fn a_refused_change_still_blocks_confirmation_after_a_restart() {
    // The refused change is skipped on the remote for good, so after a
    // restart writes that the remote does accept must still not be confirmed.
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    write(&a, "r1", 0).await;
    push(&a).await;
    server.remote_sql("DROP TABLE wasm_modules");
    a.upsert_wasm_module("default", "notes", b"", "v1", "bundled")
        .await
        .expect("upsert module");
    a.sync_copy()
        .expect("copy")
        .push_committed()
        .await
        .expect_err("the remote refused the module");
    drop(a);

    let a = server.open(&a_path).await;
    write(&a, "r1", 1).await;
    let error = a
        .sync_copy()
        .expect("copy")
        .push_committed()
        .await
        .expect_err("still not confirmed after a restart");
    assert!(error.to_string().contains("missing a change"), "{error}");
    drop(a);

    // An operator rebuilt the remote, then clears the stop with the app stopped.
    let cleared = SyncCopy::clear_stop(a_path.to_str().expect("utf-8")).expect("clear");
    assert!(cleared.is_some_and(|cause| cause.contains("wasm_modules")));
    let a = server.open(&a_path).await;
    assert_eq!(a.sync_copy().expect("copy").state(), CopyState::Copying);
    write(&a, "r1", 2).await;
    push(&a).await;
}

#[tokio::test]
async fn a_push_to_a_remote_that_stops_answering_fails_in_bounded_time() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, open) = valve(&server.url).await;
    let (_a_dir, a_path) = machine();
    let a = TursoEventStore::new_synced(a_path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("open through the valve");
    open.store(false, std::sync::atomic::Ordering::Release);
    write(&a, "s1", 0).await;
    let started = Instant::now();
    a.sync_copy()
        .expect("copy")
        .push_committed()
        .await
        .expect_err("a push nobody answers is not a success");
    assert_eq!(a.sync_copy().expect("copy").pushes_timed_out_in_a_row(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_push_given_up_on_is_checked_with_the_remote_and_a_hung_remote_still_restarts() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, open) = valve(&server.url).await;
    let (_a_dir, a_path) = machine();
    let a = TursoEventStore::new_synced(a_path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("open through the valve");
    let copy = a.sync_copy().expect("copy").clone();
    write(&a, "s1", 0).await;
    copy.push_committed()
        .await
        .expect("a push the remote answers");
    assert!(
        !copy.remote_record_unsure(),
        "a push that succeeded is known"
    );

    // An answer's time limit gives up on a push the remote may still commit.
    open.store(false, std::sync::atomic::Ordering::Release);
    write(&a, "s2", 0).await;
    copy.push_committed_within(Duration::from_millis(300))
        .await
        .expect_err("not answered in time");
    assert!(
        copy.remote_record_unsure(),
        "a push given up on may have committed: the next one asks the remote first"
    );

    // The remote answers nothing at all: asking it counts like a push that
    // timed out, so the restart after 6 in a row still comes.
    copy.push_committed()
        .await
        .expect_err("a remote that answers nothing");
    assert_eq!(copy.pushes_timed_out_in_a_row(), 1);
}
