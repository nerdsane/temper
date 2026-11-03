//! How long opening the engine may take. A restore downloads the whole
//! database, and the engine sets no timeout of its own, so a remote that
//! stops answering would hold the start forever. A large database takes as
//! long as it takes, so the open is bounded by its progress, not by a clock.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::files::{belongs_to, dir_of};

/// How long opening may go without writing to the database's files before it
/// fails. A healthy restore writes pages as they arrive; it goes quiet only
/// while the remote prepares the first page and while the written pages are
/// flushed to disk at the end. That flush can be a few GB of page cache; at
/// the slowest restore rate seen, about 11 MB/s, 3 GB takes under five
/// minutes. Failing too early costs a whole new download; failing late only
/// holds a start whose remote went quiet for a few more minutes.
pub(super) const RESTORE_STALL_BUDGET: Duration = Duration::from_secs(300);

/// How often a running open looks at the files.
pub(super) const PROGRESS_CHECK_EVERY: Duration = Duration::from_secs(5);

/// Run `opening` while `footprint` keeps changing. `None` once it has not
/// changed for [`RESTORE_STALL_BUDGET`].
pub(super) async fn while_progressing<T, S: PartialEq>(
    opening: impl Future<Output = T>,
    mut footprint: impl FnMut() -> S,
) -> Option<T> {
    let mut opening = std::pin::pin!(opening);
    let mut seen = footprint();
    let mut moved_at = tokio::time::Instant::now();
    let mut check = tokio::time::interval(PROGRESS_CHECK_EVERY);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            opened = &mut opening => return Some(opened),
            _ = check.tick() => {
                let now = footprint();
                if now != seen {
                    seen = now;
                    moved_at = tokio::time::Instant::now();
                } else if moved_at.elapsed() >= RESTORE_STALL_BUDGET {
                    return None;
                }
            }
        }
    }
}

/// Each of the database's files as it stands on disk: its length, the space
/// allocated to it and its last write. The engine sizes the file to the whole
/// database before the first page arrives, so while the pages download only
/// the allocated space and the write time move.
pub(super) fn footprint(path: &Path) -> BTreeMap<String, (u64, u64, Option<SystemTime>)> {
    let dir = dir_of(path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            if !belongs_to(&name, &file) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            Some((file, (meta.len(), allocated(&meta), meta.modified().ok())))
        })
        .collect()
}

/// Bytes of the database written so far, and its length. While a restore
/// downloads, the length is already the whole database's.
pub(super) fn written(path: &Path) -> (u64, u64) {
    std::fs::metadata(path)
        .map(|meta| {
            (
                allocated(&meta).saturating_mul(512).min(meta.len()),
                meta.len(),
            )
        })
        .unwrap_or_default()
}

/// The space allocated to the file, in 512-byte blocks.
#[cfg(unix)]
fn allocated(meta: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::blocks(meta)
}

#[cfg(not(unix))]
fn allocated(_meta: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Instant, sleep};

    #[tokio::test(start_paused = true)]
    async fn an_open_that_keeps_writing_runs_until_it_finishes() {
        let start = Instant::now();
        let opened = while_progressing(
            async {
                sleep(Duration::from_secs(1800)).await;
                "opened"
            },
            || start.elapsed().as_secs() / 30,
        )
        .await;
        assert_eq!(opened, Some("opened"), "30 minutes of steady writing");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_shorter_than_the_budget_is_not_a_stall() {
        let start = Instant::now();
        let quiet = RESTORE_STALL_BUDGET - PROGRESS_CHECK_EVERY;
        let opened = while_progressing(
            async {
                sleep(RESTORE_STALL_BUDGET * 3).await;
                "opened"
            },
            || match start.elapsed() {
                elapsed if elapsed < quiet => 0,
                elapsed => elapsed.as_secs(),
            },
        )
        .await;
        assert_eq!(opened, Some("opened"));
    }

    #[tokio::test(start_paused = true)]
    async fn an_open_that_stops_writing_fails_once_the_budget_passes() {
        let start = Instant::now();
        let wrote_until = Duration::from_secs(600);
        let opened = while_progressing(std::future::pending::<()>(), || {
            start.elapsed().min(wrote_until).as_secs()
        })
        .await;
        let waited = start.elapsed();
        assert_eq!(opened, None, "a stalled open fails");
        assert!(
            waited >= wrote_until + RESTORE_STALL_BUDGET
                && waited <= wrote_until + RESTORE_STALL_BUDGET + PROGRESS_CHECK_EVERY,
            "fails one budget after the last write, not before: {waited:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_page_written_into_a_file_already_at_full_size_counts() {
        use std::os::unix::fs::FileExt;
        // The engine sizes the file to the whole database before the first
        // page arrives, so the length stays put while the download runs.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(64 * 1024 * 1024).expect("size");
        let before = footprint(&path);
        file.write_at(&[7; 4096], 32 * 1024 * 1024).expect("page");
        file.sync_all().expect("sync");
        assert_ne!(footprint(&path), before);
    }
}
