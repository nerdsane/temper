//! The database file and the files the sync engine keeps beside it.

use std::path::{Path, PathBuf};

use super::DriverError;

/// Written next to the database when the remote refused a change.
pub(super) const DIVERGED_SUFFIX: &str = "-sync-diverged";

/// Written next to the database while it is restored from the remote, and
/// removed once the restore has finished. A file found with it holds only part
/// of the remote, and nothing of its own.
pub(super) const RESTORING_SUFFIX: &str = "-sync-restoring";

/// Move the database file and every file the engine keeps beside it to
/// `<name>.behind-<unix seconds>`, so the next open restores from the remote.
pub(super) fn move_aside(path: &Path) -> Result<PathBuf, DriverError> {
    let failed =
        |error: std::io::Error| DriverError::SyncSetup(format!("move {}: {error}", path.display()));
    let dir = dir_of(path);
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let stamp = std::time::SystemTime::now() // determinism-ok: names a file moved aside once
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let aside = dir.join(format!("{name}.behind-{stamp}"));
    std::fs::create_dir_all(&aside).map_err(failed)?;
    for entry in std::fs::read_dir(dir).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let file = entry.file_name().to_string_lossy().into_owned();
        if belongs_to(&name, &file) {
            std::fs::rename(entry.path(), aside.join(&file)).map_err(failed)?;
        }
    }
    Ok(aside)
}

/// The directory that holds `path` and the files kept beside it. A bare file
/// name is in the working directory (its parent is the empty path).
pub(super) fn dir_of(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Whether `file` in the database's directory is the database `name` or one of
/// the files kept beside it (`<name>-info`, `-wal`, the markers here).
pub(super) fn belongs_to(name: &str, file: &str) -> bool {
    file == name || file.starts_with(&format!("{name}-"))
}

/// Written next to the database while it waits to be checked against the
/// remote: the newest change the file held before that start committed
/// anything. A later start keeps the number, since what it commits meanwhile
/// says nothing about whether the file is behind.
const UNCHECKED_SUFFIX: &str = "-sync-unchecked";

pub(super) fn unchecked_since(path: &Path) -> Option<i64> {
    std::fs::read_to_string(sidecar(path, UNCHECKED_SUFFIX))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub(super) fn mark_unchecked(path: &Path, opened_at: i64) -> Result<(), DriverError> {
    if unchecked_since(path).is_some() {
        return Ok(());
    }
    std::fs::write(sidecar(path, UNCHECKED_SUFFIX), opened_at.to_string())
        .map_err(|error| DriverError::SyncSetup(format!("record the unchecked start: {error}")))
}

pub(super) fn mark_checked(path: &Path) {
    let _ = std::fs::remove_file(sidecar(path, UNCHECKED_SUFFIX));
}

/// The sync engine's metadata for the file (`<db>-info`), read once. Only for
/// a file no engine is running on: at open, and in an operator's report.
pub(super) fn sync_metadata(path: &Path) -> Result<serde_json::Value, DriverError> {
    let info = std::fs::read(sidecar(path, "-info"))
        .map_err(|error| DriverError::SyncSetup(format!("read sync metadata: {error}")))?;
    serde_json::from_slice(&info)
        .map_err(|error| DriverError::SyncSetup(format!("parse sync metadata: {error}")))
}

/// The sync engine's metadata for a file the engine is running on.
///
/// The engine rewrites `<db>-info` in place after every push, pull and
/// checkpoint: `tokio::fs::write`, which truncates the file and then writes
/// it (turso 0.7.2 `sync.rs` 912-930, turso_sync_engine 0.7.2
/// `database_sync_engine.rs` 3177-3197). It does that on its own IO thread,
/// which finishes a write it has started even when the operation that asked
/// for it was dropped by a timeout. So a reader can find the file empty or
/// half written at any time, and the engine has no call that returns what
/// the file holds. A read that finds an incomplete file reads it again until
/// it parses: a prefix of a JSON object never parses, so what parses is one
/// whole version.
///
/// A missing file fails at once: no rewrite removes it. A record still
/// incomplete after [`REWRITE_BUDGET`] fails with what the last read found.
pub(super) async fn read_sync_metadata(path: &Path) -> Result<serde_json::Value, DriverError> {
    let started = std::time::Instant::now();
    loop {
        match sync_metadata(path) {
            Ok(info) => return Ok(info),
            Err(error) if !sidecar(path, "-info").exists() => return Err(error),
            Err(error) if started.elapsed() >= REWRITE_BUDGET => return Err(error),
            Err(_) => tokio::time::sleep(REWRITE_POLL).await,
        }
    }
}

/// How long a read waits for the engine to finish rewriting its record. The
/// rewrite is one write of a few hundred bytes, so a record incomplete for
/// this long is not being rewritten but broken, and the push that needs it
/// fails with the cause. It stays well inside an answer's 10 s for its push.
const REWRITE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// How often a read looks at a record that was incomplete.
const REWRITE_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// Remove the recorded stop, returning the cause it held.
pub(super) fn clear_stop(path: &Path) -> Result<Option<String>, DriverError> {
    let marker = sidecar(path, DIVERGED_SUFFIX);
    let Ok(cause) = std::fs::read_to_string(&marker) else {
        return Ok(None);
    };
    let cause = cause.trim().to_string();
    std::fs::remove_file(&marker)
        .map_err(|error| DriverError::SyncSetup(format!("remove {}: {error}", marker.display())))?;
    Ok(Some(cause))
}

/// A short account of the file, its sync metadata and any stop.
pub(super) fn describe(path: &Path) -> Result<String, DriverError> {
    let size = |file: &Path| std::fs::metadata(file).map(|meta| meta.len()).ok();
    let mut lines = vec![match size(path) {
        Some(bytes) => format!("database: {} ({bytes} bytes)", path.display()),
        None => format!(
            "database: {} (missing; the next start restores it from the remote)",
            path.display()
        ),
    }];
    match sync_metadata(path) {
        Ok(info) => lines.push(format!(
            "synced: client {}, last change the remote confirmed {}",
            info["client_unique_id"].as_str().unwrap_or("?"),
            info["last_pushed_change_id_hint"].as_i64().unwrap_or(0)
        )),
        Err(_) => lines.push("synced: no sync metadata".to_string()),
    }
    if sidecar(path, RESTORING_SUFFIX).exists() {
        lines.push("restoring: a restore from the remote started and has not finished".to_string());
    }
    if let Some(opened_at) = unchecked_since(path) {
        lines.push(format!(
            "unchecked: since a start with changes up to {opened_at}"
        ));
    }
    lines.push(
        match std::fs::read_to_string(sidecar(path, DIVERGED_SUFFIX)) {
            Ok(cause) => format!("stopped: {}", cause.trim()),
            Err(_) => "stopped: no".to_string(),
        },
    );
    Ok(lines.join("\n"))
}

/// Refuse to open a local database that the sync engine would replace.
///
/// With no sync metadata next to it, the engine bootstraps the file from the
/// remote copy, which truncates whatever the file held. That is right for a
/// fresh machine (no file, or an empty one) and wrong for a database that was
/// written before sync was turned on: its rows were never sent anywhere.
///
/// A file with the restore's marker is a restore that stopped part way: only
/// some of the remote's pages. It is moved aside, kept, and restored again.
pub(super) fn check_local_files(path: &Path) -> Result<(), DriverError> {
    if sidecar(path, "-info").exists() {
        return Ok(());
    }
    let holds_data = |file: &Path| std::fs::metadata(file).is_ok_and(|meta| meta.len() > 0);
    let wal = sidecar(path, "-wal");
    if !holds_data(path) && !holds_data(&wal) {
        return Ok(());
    }
    if sidecar(path, RESTORING_SUFFIX).exists() {
        let moved = move_aside(path)?;
        tracing::warn!(
            moved = %moved.display(),
            "turso.sync a restore from the remote did not finish; restoring again"
        );
        return Ok(());
    }
    Err(DriverError::SyncSetup(format!(
        "{} holds data that was never copied to the sync database, and opening it with \
         sync would replace it with the remote copy. Copy the file to the remote database \
         first, or move it away to restore from the remote copy",
        path.display()
    )))
}

/// Mark the start of a restore: the engine is about to fill the file from the
/// remote. [`restored`] removes the mark once the open has finished.
pub(super) fn mark_restoring(path: &Path) -> Result<(), DriverError> {
    std::fs::write(sidecar(path, RESTORING_SUFFIX), b"")
        .map_err(|error| DriverError::SyncSetup(format!("record the restore: {error}")))
}

/// Clear the mark of a restore that has finished.
pub(super) fn restored(path: &Path) {
    let _ = std::fs::remove_file(sidecar(path, RESTORING_SUFFIX));
}

/// Refuse a fetch into `path` when the database or any file kept beside it is
/// already there: a fetch never overwrites.
pub(super) fn refuse_existing(path: &Path) -> Result<(), DriverError> {
    let dir = dir_of(path);
    let failed =
        |error: std::io::Error| DriverError::SyncSetup(format!("read {}: {error}", dir.display()));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for entry in std::fs::read_dir(dir).map_err(failed)? {
        let file = entry.map_err(failed)?.file_name();
        if belongs_to(&name, &file.to_string_lossy()) {
            return Err(DriverError::SyncSetup(format!(
                "{} already exists; a fetch never overwrites, so fetch into a new path",
                dir.join(file).display()
            )));
        }
    }
    Ok(())
}

pub(super) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_local_file_written_before_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(&path, b"SQLite format 3\0").expect("write db");

        let error = check_local_files(&path).expect_err("must refuse");
        assert!(error.to_string().contains("never copied"), "{error}");
    }

    #[test]
    fn refuses_a_wal_written_before_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(&path, b"").expect("write db");
        std::fs::write(sidecar(&path, "-wal"), b"frames").expect("write wal");

        assert!(check_local_files(&path).is_err());
    }

    #[test]
    fn opens_a_missing_or_empty_file_and_a_synced_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        check_local_files(&path).expect("missing file restores from the remote");

        std::fs::write(&path, b"").expect("write db");
        check_local_files(&path).expect("empty file restores from the remote");

        std::fs::write(&path, b"SQLite format 3\0").expect("write db");
        std::fs::write(sidecar(&path, "-info"), b"{}").expect("write info");
        check_local_files(&path).expect("a synced file opens as it is");
    }

    #[test]
    fn a_restore_cut_short_is_moved_aside_and_run_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(&path, b"SQLite format 3\0part of the remote").expect("write db");
        std::fs::write(sidecar(&path, RESTORING_SUFFIX), b"").expect("write marker");

        check_local_files(&path).expect("a partial restore is restored again");
        assert!(!path.exists(), "the next restore starts from no file");
        let aside = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").path())
            .find(|entry| entry.is_dir())
            .expect("the partial file is kept aside");
        assert_eq!(
            std::fs::read(aside.join("app.db")).expect("kept"),
            b"SQLite format 3\0part of the remote"
        );
        assert!(
            aside.join("app.db-sync-restoring").exists(),
            "the marker goes with the file it describes"
        );
    }

    #[test]
    fn a_restore_that_wrote_nothing_or_finished_stays_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(sidecar(&path, RESTORING_SUFFIX), b"").expect("write marker");
        check_local_files(&path).expect("nothing was written, so nothing to keep");
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("read dir").count(),
            1,
            "only the marker"
        );

        // The engine writes its metadata only after the last page is on disk.
        std::fs::write(&path, b"SQLite format 3\0").expect("write db");
        std::fs::write(sidecar(&path, "-info"), b"{}").expect("write info");
        check_local_files(&path).expect("a finished restore opens as it is");
        assert!(path.exists());
    }

    /// The engine's rewrite, slowed so a reader lands inside it: the file is
    /// truncated, stays empty for a moment, then holds the whole record.
    fn rewrite_in_place(info: &Path, record: &[u8]) {
        use std::io::Write;
        let mut file = std::fs::File::create(info).expect("truncate info");
        std::thread::sleep(std::time::Duration::from_millis(2));
        file.write_all(record).expect("write info");
    }

    #[tokio::test]
    async fn the_engines_record_is_read_whole_while_the_engine_rewrites_it() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let info = sidecar(&path, "-info");
        let record = br#"{"client_unique_id":"temper-1","last_pushed_change_id_hint":157}"#;
        std::fs::write(&info, record).expect("write info");

        let stop = Arc::new(AtomicBool::new(false));
        let engine = {
            let (info, stop) = (info.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    rewrite_in_place(&info, record);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            })
        };
        let mut reads = Vec::new();
        for _ in 0..200 {
            reads.push(read_sync_metadata(&path).await);
            tokio::time::sleep(std::time::Duration::from_micros(300)).await;
        }
        stop.store(true, Ordering::Release);
        engine.join().expect("engine thread");

        let failed: Vec<String> = reads
            .iter()
            .filter_map(|read| read.as_ref().err().map(ToString::to_string))
            .collect();
        assert!(
            failed.is_empty(),
            "{} of 200 reads failed during rewrites, first: {}",
            failed.len(),
            failed[0]
        );
        for read in reads {
            assert_eq!(read.expect("read")["last_pushed_change_id_hint"], 157);
        }
    }

    #[tokio::test]
    async fn a_missing_record_is_an_error_at_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let started = std::time::Instant::now();
        let error = read_sync_metadata(&path).await.expect_err("no record");
        assert!(error.to_string().contains("read sync metadata"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "a missing file is not waited on: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_fetch_never_lands_on_an_existing_database_or_its_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(dir.path().join("other.db"), b"x").expect("write other");
        std::fs::create_dir(dir.path().join("app.db.behind-1")).expect("aside dir");
        refuse_existing(&path).expect("nothing of app.db is here");

        for file in [
            "app.db",
            "app.db-info",
            "app.db-wal",
            "app.db-sync-restoring",
        ] {
            let there = dir.path().join(file);
            std::fs::write(&there, b"").expect("write");
            let error = refuse_existing(&path).expect_err(file);
            assert!(error.to_string().contains(file), "{error}");
            std::fs::remove_file(&there).expect("remove");
        }
    }
}
