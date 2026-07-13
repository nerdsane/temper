//! End-to-end verification of the Phase 4 blob TTL + sweep primitives against
//! a real local libsql/Turso database (file-backed).
//!
//! Covers ADR-0047:
//! - `put_blob` writes with NULL expires_at (permanent).
//! - `put_blob_with_ttl(..., Some(d))` sets `expires_at` to a future wall-clock.
//! - `sweep_expired_blobs` deletes past-expires_at rows and ignores NULL rows.
//! - The schema migration `ALTER TABLE blobs ADD COLUMN expires_at TEXT` is
//!   idempotent across reopens.

use std::time::Duration;
use temper_store_turso::TursoEventStore;
use tempfile::TempDir;

async fn open_store() -> (TursoEventStore, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("e2e.db");
    let url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("TursoEventStore::new");
    (store, dir)
}

#[tokio::test]
async fn put_blob_writes_permanent_row() {
    let (store, _dir) = open_store().await;
    store
        .put_blob("field-overflow/sha256/perm.json", b"hello")
        .await
        .expect("put_blob");
    let fetched = store
        .get_blob("field-overflow/sha256/perm.json")
        .await
        .expect("get_blob")
        .expect("row present");
    assert_eq!(fetched, b"hello");
}

#[tokio::test]
async fn put_blob_with_ttl_round_trips_content() {
    let (store, _dir) = open_store().await;
    store
        .put_blob_with_ttl(
            "field-overflow/sha256/ttl.json",
            b"with-ttl",
            Some(Duration::from_secs(60)),
        )
        .await
        .expect("put_blob_with_ttl");
    let fetched = store
        .get_blob("field-overflow/sha256/ttl.json")
        .await
        .expect("get_blob")
        .expect("row present");
    assert_eq!(fetched, b"with-ttl");
}

#[tokio::test]
async fn sweep_removes_expired_rows_and_preserves_permanent() {
    let (store, _dir) = open_store().await;

    // Permanent row (put_blob -> NULL expires_at).
    store
        .put_blob("field-overflow/sha256/perm.json", b"permanent")
        .await
        .expect("put_blob permanent");

    // Row with TTL of 0 seconds — `datetime('now', '+0 seconds')` is exactly
    // "now" at write time; by the time the sweep runs the string comparison
    // resolves to past-or-equal, which the `<` in the sweep predicate treats
    // as not-yet-expired. Use a small sleep to guarantee strict `<`.
    store
        .put_blob_with_ttl(
            "field-overflow/sha256/expired.json",
            b"expired",
            Some(Duration::from_secs(1)),
        )
        .await
        .expect("put_blob_with_ttl expired");

    // Row with a long TTL that must survive.
    store
        .put_blob_with_ttl(
            "field-overflow/sha256/future.json",
            b"future",
            Some(Duration::from_secs(3600)),
        )
        .await
        .expect("put_blob_with_ttl future");

    // Wait past the 1-second TTL with enough margin for SQLite's
    // second-granularity `datetime('now')` to have advanced.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let deleted = store
        .sweep_expired_blobs(1000)
        .await
        .expect("sweep_expired_blobs");
    assert_eq!(deleted, 1, "exactly the expired row should be deleted");

    assert!(
        store
            .get_blob("field-overflow/sha256/perm.json")
            .await
            .expect("get perm")
            .is_some(),
        "permanent row must survive sweep"
    );
    assert!(
        store
            .get_blob("field-overflow/sha256/expired.json")
            .await
            .expect("get expired")
            .is_none(),
        "expired row must be gone"
    );
    assert!(
        store
            .get_blob("field-overflow/sha256/future.json")
            .await
            .expect("get future")
            .is_some(),
        "future-TTL row must survive sweep"
    );
}

#[tokio::test]
async fn sweep_noop_when_all_rows_permanent() {
    let (store, _dir) = open_store().await;
    store
        .put_blob("field-overflow/sha256/a.json", b"a")
        .await
        .unwrap();
    store
        .put_blob("field-overflow/sha256/b.json", b"b")
        .await
        .unwrap();

    let deleted = store
        .sweep_expired_blobs(1000)
        .await
        .expect("sweep_expired_blobs");
    assert_eq!(deleted, 0, "permanent rows are untouched by sweep");
    assert!(
        store
            .get_blob("field-overflow/sha256/a.json")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get_blob("field-overflow/sha256/b.json")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn stamped_database_reopens_cleanly_and_preserves_blobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("e2e.db");
    let url = format!("file:{}", db_path.display());

    // First open — runs CREATE + ALTER (which is a no-op because CREATE
    // already includes expires_at). Write a row with a permanent blob so
    // we can verify it survives the second open.
    {
        let store = TursoEventStore::new(&url, None).await.expect("first open");
        store
            .put_blob("field-overflow/sha256/survive.json", b"survive")
            .await
            .expect("put_blob");
    }

    // Second open — the ARN-242 ledger short-circuits: the database is already
    // stamped at the current SCHEMA_VERSION, so migrate() skips the DDL entirely
    // and the store opens cleanly against the existing schema; the blob written
    // before the reopen must still be readable.
    {
        let store = TursoEventStore::new(&url, None).await.expect("second open");
        let bytes = store
            .get_blob("field-overflow/sha256/survive.json")
            .await
            .expect("get_blob")
            .expect("row present");
        assert_eq!(bytes, b"survive");
    }
}
