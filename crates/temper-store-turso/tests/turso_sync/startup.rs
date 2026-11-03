//! Starting a synced database: without the remote, from an older copy, from
//! a restore cut short, by fetching, and from a database that was never synced.

use std::path::PathBuf;

use super::support::*;
use temper_store_turso::{CopyState, SyncCopy, TursoEventStore};

#[tokio::test]
async fn a_start_without_the_remote_serves_reads_and_confirms_writes_once_it_answers() {
    // Rita, 17 Oct: "Read only". Whether the file is behind the remote can be
    // told only by asking the remote, so until then nothing is pushed.
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, on) = switch(&server.url).await;
    let (_a_dir, a_path) = machine();
    let a = TursoEventStore::new_synced(a_path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("open");
    write(&a, "u1", 0).await;
    push(&a).await;
    drop(a);

    on.store(false, std::sync::atomic::Ordering::Release);
    let a = TursoEventStore::new_synced(a_path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("a start without the remote still serves");
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(copy.state(), CopyState::Unchecked);
    assert_eq!(count(&a, "u1").await, 1, "reads are served");
    copy.push_committed()
        .await
        .expect_err("nothing is confirmed unchecked");
    copy.recheck().await.expect_err("still unreachable");

    on.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(copy.recheck().await.expect("recheck"), CopyState::Copying);
    write(&a, "u1", 1).await;
    push(&a).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "u1").await, 2);
}

#[tokio::test]
async fn a_start_that_adds_an_index_serves_before_the_remote_builds_it() {
    // A start that adds to the schema, as an upgrade adds an index over
    // every record, asks the remote only whether the file is behind. The
    // remote builds the index, which takes as long as the app is big, with
    // the next push, while the server serves.
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, requests) = counting(&server.url).await;
    let (_a_dir, a_path) = machine();
    let open = || TursoEventStore::new_synced(a_path.to_str().expect("utf-8"), &url, TOKEN);
    let a = open().await.expect("open");
    write(&a, "i", 0).await;
    a.sync_copy()
        .expect("copy")
        .execute_local("DROP INDEX idx_snapshots_checkpoint")
        .await
        .expect("drop the index");
    push(&a).await;
    drop(a);

    let before = requests.load(std::sync::atomic::Ordering::Acquire);
    let a = open().await.expect("restart");
    assert_eq!(a.sync_copy().expect("copy").state(), CopyState::Copying);
    let asked = requests.load(std::sync::atomic::Ordering::Acquire) - before;
    assert_eq!(
        asked, 1,
        "a start that adds an index sent {asked} requests to the remote before serving"
    );
    push(&a).await;
    // The index is on the remote again: dropping it there succeeds.
    server.remote_sql("DROP INDEX idx_snapshots_checkpoint");
}

#[tokio::test]
async fn an_older_copy_opened_without_the_remote_is_restored_once_it_answers() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, on) = switch(&server.url).await;
    let open = |path: PathBuf| {
        let url = url.clone();
        async move {
            TursoEventStore::new_synced(path.to_str().expect("utf-8"), &url, TOKEN)
                .await
                .expect("open")
        }
    };
    let (a_dir, a_path) = machine();
    let older = tempfile::tempdir().expect("snapshot dir");
    let a = open(a_path.clone()).await;
    write(&a, "old", 0).await;
    push(&a).await;
    drop(a);
    copy_files(a_dir.path(), older.path());
    let a = open(a_path.clone()).await;
    for i in 0..5 {
        write(&a, "later", i).await;
    }
    push(&a).await;
    drop(a);
    remove_files(a_dir.path());
    copy_files(older.path(), a_dir.path());

    on.store(false, std::sync::atomic::Ordering::Release);
    let a = open(a_path.clone()).await;
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(copy.state(), CopyState::Unchecked);
    // Work the server does on its own still commits locally while unchecked,
    // here more changes than the remote is ahead by, so the file's newest
    // change number passes the remote's: only the number at open tells.
    for i in 0..10 {
        write(&a, "meanwhile", i).await;
    }
    // A restart during the outage keeps the number from the first start.
    drop((a, copy));
    let a = open(a_path.clone()).await;
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(copy.state(), CopyState::Unchecked);
    on.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(copy.recheck().await.expect("recheck"), CopyState::Behind);
    copy.push_committed()
        .await
        .expect_err("a file behind confirms nothing");
    drop((a, copy));

    // The restart the server makes restores the file from the remote.
    let a = open(a_path).await;
    assert_eq!(a.sync_copy().expect("copy").state(), CopyState::Copying);
    assert_eq!(count(&a, "later").await, 5);
    write(&a, "after", 0).await;
    push(&a).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "after").await, 1);
}

#[tokio::test]
async fn a_restore_cut_short_is_kept_aside_and_run_again_at_the_next_start() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    write(&a, "whole", 0).await;
    push(&a).await;
    drop(a);

    // What a start stopped part way through a restore leaves: some of the
    // remote's pages, the restore's marker, and no sync metadata.
    let (b_dir, b_path) = machine();
    std::fs::write(&b_path, vec![7u8; 64 * 1024]).expect("partial file");
    std::fs::write(b_dir.path().join("app.db-sync-restoring"), b"").expect("marker");

    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "whole").await, 1, "the restore ran again in full");
    assert!(
        !b_dir.path().join("app.db-sync-restoring").exists(),
        "a finished restore clears its marker"
    );
    let kept = std::fs::read_dir(b_dir.path())
        .expect("read dir")
        .map(|entry| entry.expect("entry").path())
        .find(|entry| entry.is_dir())
        .expect("the partial file is kept aside");
    assert_eq!(
        std::fs::metadata(kept.join("app.db")).expect("kept").len(),
        64 * 1024
    );
}

#[tokio::test]
async fn a_fetch_downloads_the_remote_beside_and_never_over_a_database() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    write(&a, "fetched", 0).await;
    push(&a).await;
    drop(a);

    let (b_dir, b_path) = machine();
    let b = b_path.to_str().expect("utf-8");
    let report = SyncCopy::fetch(b, &server.url, TOKEN).await.expect("fetch");
    assert!(report.contains("synced: client"), "{report}");
    assert!(b_dir.path().join("app.db-info").exists(), "{report}");
    assert!(
        !b_dir.path().join("app.db-sync-restoring").exists(),
        "a finished fetch is not a partial restore"
    );

    let error = SyncCopy::fetch(b, &server.url, TOKEN)
        .await
        .expect_err("a second fetch would land on the first");
    assert!(error.to_string().contains("already exists"), "{error}");

    // Moved into place by hand, the fetched file opens as it is.
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "fetched").await, 1);
    assert_eq!(b.sync_copy().expect("copy").state(), CopyState::Copying);
    let aside = std::fs::read_dir(b_dir.path())
        .expect("read dir")
        .filter(|entry| entry.as_ref().expect("entry").path().is_dir())
        .count();
    assert_eq!(aside, 0, "nothing was restored again");
}

#[tokio::test]
async fn a_database_written_before_sync_is_adopted_onto_an_empty_remote() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (a_dir, a_path) = machine();
    // A file from before primary keys: specs keyed by UNIQUE only.
    {
        let db = turso::Builder::new_local(a_path.to_str().expect("utf-8"))
            .build()
            .await
            .expect("old database");
        db.connect()
            .expect("connect")
            .execute(
                "CREATE TABLE specs (tenant TEXT NOT NULL, entity_type TEXT NOT NULL, \
                 ioa_source TEXT NOT NULL, csdl_xml TEXT, version INTEGER NOT NULL DEFAULT 1, \
                 verified INTEGER NOT NULL DEFAULT 0, verification_status TEXT NOT NULL DEFAULT 'pending', \
                 levels_passed INTEGER, levels_total INTEGER, verification_result TEXT, \
                 updated_at TEXT NOT NULL DEFAULT (datetime('now')), UNIQUE(tenant, entity_type))",
                (),
            )
            .await
            .expect("old specs table");
    }
    let plain = TursoEventStore::new(&format!("file:{}", a_path.display()), None)
        .await
        .expect("plain store");
    for i in 0..3 {
        write(&plain, "kept", i).await;
    }
    plain
        .upsert_specs_and_commit("default", &[("Note", "v1", "<csdl/>", "v1")], None, "notes")
        .await
        .expect("spec");
    drop(plain);

    let unreachable = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", listener.local_addr().expect("addr"))
    };
    let db = a_path.to_str().expect("utf-8");
    temper_store_turso::store::sync_adopt::adopt(db, &unreachable, TOKEN)
        .await
        .expect_err("no remote, no adoption");
    assert!(a_path.exists(), "a failed adoption puts the file back");
    assert!(!a_dir.path().join("app.db-info").exists());

    // A run that began copying, then was killed or failed, pushed some rows.
    let staging = a_dir.path().join("app.db.adopting");
    let killed = server.open(&staging).await;
    write(&killed, "stray", 0).await;
    push(&killed).await;
    drop(killed);
    temper_store_turso::store::sync_adopt::adopt(db, &server.url, TOKEN)
        .await
        .expect_err("a remote holding events no run of this adopt began on is refused");
    let started = a_dir.path().join("app.db.adopt-started");
    std::fs::write(&started, b"").expect("mark");

    let report = temper_store_turso::store::sync_adopt::adopt(db, &server.url, TOKEN)
        .await
        .expect("adopt, starting over after the run that began copying");
    assert!(!started.exists(), "the mark goes once the file is moved");
    assert!(
        !staging.exists(),
        "the staged database took the file's place"
    );
    assert!(
        std::fs::read_dir(a_dir.path())
            .expect("dir")
            .flatten()
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("app.db.before-sync-")),
        "the original is kept aside"
    );
    assert!(report.contains("specs: 1 rows"), "{report}");
    temper_store_turso::store::sync_adopt::adopt(db, &server.url, TOKEN)
        .await
        .expect_err("already synced");

    let a = server.open(&a_path).await;
    assert_eq!(count(&a, "kept").await, 3);
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "kept").await, 3, "the rows reached the remote");
    let specs = b.load_specs().await.expect("specs");
    assert!(
        specs
            .iter()
            .any(|spec| spec.entity_type == "Note" && spec.ioa_source == "v1")
    );
}

#[tokio::test]
async fn reopening_versioned_secrets_adds_no_change_log_rows() {
    use temper_runtime::persistence::{SecretPrecondition, SecretWrite};

    let Some(server) = SyncServer::start() else {
        return;
    };
    for with_secret in [false, true] {
        let (_dir, path) = machine();
        let store = server.open(&path).await;
        if with_secret {
            assert!(
                store
                    .write_secret(&SecretWrite {
                        tenant: "tenant-a",
                        key_name: "signin",
                        ciphertext: b"encrypted",
                        nonce: b"nonce",
                        version_id: "original-version",
                        precondition: SecretPrecondition::Absent,
                    })
                    .await
                    .expect("write secret")
            );
        }
        push(&store).await;
        let versions = store.list_secret_versions("tenant-a").await.unwrap();
        let copy = store.sync_copy().expect("copy");
        let rows = copy.change_log_rows().await.unwrap();
        let newest = copy.last_change().await.unwrap();
        drop(store);

        for _ in 0..2 {
            let store = server.open(&path).await;
            let copy = store.sync_copy().expect("copy");
            assert_eq!(
                copy.change_log_rows().await.unwrap(),
                rows,
                "a secret backfill with no missing versions must not write to the change log"
            );
            assert_eq!(copy.last_change().await.unwrap(), newest);
            assert_eq!(
                store.list_secret_versions("tenant-a").await.unwrap(),
                versions
            );
        }
    }
}

#[tokio::test]
async fn reopening_completed_creation_backfill_adds_no_change_log_rows() {
    use temper_runtime::persistence::EventStore;

    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let store = server.open(&path).await;
    let created = event(1);
    let expected = created
        .metadata
        .timestamp
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .append("default:Note:legacy", 0, &[created])
        .await
        .unwrap();
    store
        .upsert_query_projection(
            "default",
            "Note",
            "legacy",
            "Created",
            &serde_json::json!({}),
            1,
        )
        .await
        .unwrap();
    // Model a pre-migration catalog with a pending durable cursor.
    store
        .sync_copy()
        .unwrap()
        .execute_local("UPDATE first_event_backfill SET complete = 0 WHERE id = 1")
        .await
        .unwrap();
    push(&store).await;
    drop(store);

    let store = server.open(&path).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let rows = store
                .load_entity_catalog_rows("default", "Note", &["legacy".into()])
                .await
                .unwrap();
            if rows[0].state.as_ref().unwrap()["first_event_at"] == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("creation backfill completes");
    push(&store).await;
    let copy = store.sync_copy().unwrap();
    let rows = copy.change_log_rows().await.unwrap();
    let newest = copy.last_change().await.unwrap();
    drop(store);

    for _ in 0..2 {
        let store = server.open(&path).await;
        let copy = store.sync_copy().unwrap();
        assert_eq!(
            copy.change_log_rows().await.unwrap(),
            rows,
            "a completed creation backfill must not write to the change log"
        );
        assert_eq!(copy.last_change().await.unwrap(), newest);
        let catalog = store
            .load_entity_catalog_rows("default", "Note", &["legacy".into()])
            .await
            .unwrap();
        assert_eq!(
            catalog[0].state.as_ref().unwrap()["first_event_at"],
            expected
        );
    }
}
