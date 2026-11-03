//! Trimming the change log the remote has confirmed, off every answer's path.

use std::future::Future;
use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{Connection, DriverError, SyncCopy};

/// The least row work a batch is planned for. On a drive whose commit takes
/// longer, a batch's rows work as long as its commit instead (see
/// [`next_plan`]). A write that arrives during a batch waits for it. A batch
/// is planned from the last one's rate, and may double it, so one fast batch
/// followed by the usual rate runs twice this long: on CI, a 100 ms target
/// ran a 170 ms batch. Half of 100 ms keeps that case near 100 ms when
/// commits are quick.
const BATCH_TARGET: Duration = Duration::from_millis(50);

/// The first batch's plan. At the slowest rate measured, a cold log on a
/// Tensorlake drive (10,000 rows in 34.2 s, about 290 rows a second), 12
/// rows take about 41 ms, within [`BATCH_TARGET`].
pub(super) const FIRST_PLAN: Plan = Plan {
    rows: 12,
    cut_after: Duration::from_millis(2 * BATCH_TARGET.as_millis() as u64),
};

/// How often a batch whose DELETE has run past its plan looks for a waiting
/// write. Short against one commit on a slow drive (about 300 ms on tlfs,
/// roll3, 26 Oct 2026).
const WRITER_RECHECK: Duration = Duration::from_millis(25);

/// The writers a trim batch shares the database with (the store's write
/// gate). A batch must never keep one of them waiting longer than it has to.
pub(crate) trait Writers {
    /// Held through one batch: no writer starts while it is held.
    type Turn: Send;
    /// A turn, given only while no writer waits.
    fn turn(&self) -> impl Future<Output = Result<Self::Turn, PersistenceError>> + Send;
    /// Whether a writer is waiting now.
    fn waiting(&self) -> bool;
    /// How long a write waits for the database's write lock before it gives
    /// up and its move is refused.
    fn lock_wait(&self) -> Duration;
}

/// What the next batch deletes, and how long its DELETE may run while a
/// write waits: twice what it is expected to take, since a batch planned from
/// the last one's rate runs up to about that long when its rows are colder
/// (on CI a 100 ms plan ran 170 ms), and at least [`BATCH_TARGET`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Plan {
    rows: i64,
    cut_after: Duration,
}

/// What one batch did and what it cost.
struct Batch {
    /// Rows it was planned to delete.
    planned: i64,
    deleted: u64,
    /// Stopped for a waiting write and rolled back: nothing was deleted.
    cut: bool,
    /// The DELETE: the rows' own work.
    work: Duration,
    /// The COMMIT. On a slow drive this is mostly a sync to the disk that
    /// costs the same whatever the rows.
    commit: Duration,
    /// From BEGIN to its end: how long writes could not start.
    held: Duration,
}

impl SyncCopy {
    /// Delete change log rows the remote has confirmed, for about `budget`,
    /// keeping the newest row. Returns the rows deleted.
    ///
    /// Each batch runs under a turn from `writers`, given only while no
    /// writer waits and holding every writer back until the batch ends. So a
    /// write waiting when a batch would start goes first, and one that
    /// arrives during a batch waits for that batch only: its commit and at
    /// most twice its DELETE's expected time, since a DELETE still running
    /// past that while a write waits is stopped and rolled back (its rows
    /// wait for a smaller batch). While writes keep the database busy the trim gets no
    /// turn, and a call that got none before `budget` ran out ends.
    ///
    /// In 'full' mode each row holds whole row images, and nothing else
    /// trims the log, so it grows with every write. Only the background loop
    /// calls this: an answer's push never starts a trim.
    ///
    /// The engine reads the log only above the remote's record of this
    /// file's last push: a push scans from that record plus one
    /// (turso_sync_engine 0.7.2 `database_sync_operations.rs` 2853,
    /// `database_tape.rs` 416-432), a pull replays only rows above it
    /// (`database_sync_engine.rs` 864-887), and its stats count rows above
    /// it (1405-1427). The record trimmed to is the one the engine saved
    /// after the remote committed, so no row at or below it is read again.
    ///
    /// No push lock is held. A push reads only rows above the remote's
    /// record, which is at or past the saved one, and a commit adds only
    /// rows numbered above the newest, so a push in flight and writes that
    /// land meanwhile touch none of the rows a batch deletes.
    ///
    /// The newest row stays because in WAL mode a change is numbered one
    /// above the largest in the log (turso_core 0.7.2
    /// `translate/emitter/mod.rs` 1223-1239, `vdbe/execute.rs` 10823-10841).
    /// An empty log would number the next change 1, at or below what the
    /// remote holds, and the next push would skip it. [`Self::last_change`],
    /// the start's check against the remote and the check after each push
    /// read that same newest row.
    ///
    /// Each batch is one transaction: BEGIN IMMEDIATE, one DELETE of the
    /// oldest rows, COMMIT. Batches are planned by their rows' cost
    /// ([`next_plan`]), so a slow commit makes them bigger, not one row
    /// each. A batch starts only within `budget`, and a COMMIT is never cut
    /// off. A failed call leaves the rows for the next.
    pub(crate) async fn trim_pushed<W: Writers + Sync>(
        &self,
        budget: Duration,
        writers: &W,
    ) -> Result<u64, PersistenceError> {
        self.require_copying()?;
        let started = Instant::now();
        let confirmed = self.last_pushed_hint().await.map_err(storage_error)?;
        let conn = self.connect().await.map_err(storage_error)?;
        let mut trimmed = 0u64;
        let mut batches = 0u32;
        let mut cuts = 0u32;
        let mut held = Duration::ZERO;
        let mut longest = Duration::ZERO;
        let mut last: Option<Batch> = None;
        let (through, oldest, newest) = loop {
            // Read before the turn: only this loop deletes rows, so the oldest
            // stays put, and the newest only grows while it waits.
            let Some((oldest, newest)) = change_ids(&conn).await.map_err(storage_error)? else {
                return Ok(trimmed);
            };
            let through = confirmed.min(newest - 1);
            let left = budget.saturating_sub(started.elapsed());
            if through < oldest || left.is_zero() {
                break (through, oldest, newest);
            }
            let Ok(turn) = tokio::time::timeout(left, writers.turn()).await else {
                break (through, oldest, newest);
            };
            let turn = turn?;
            let plan = *self.inner.trim_plan.lock().unwrap();
            let upper = through.min(oldest.saturating_add(plan.rows - 1));
            let batch = self.delete_through(&conn, upper, plan, writers).await;
            drop(turn);
            let batch = batch.map_err(storage_error)?;
            *self.inner.trim_plan.lock().unwrap() = next_plan(&batch, writers.lock_wait());
            trimmed += batch.deleted;
            batches += 1;
            cuts += u32::from(batch.cut);
            held += batch.held;
            longest = longest.max(batch.held);
            last = Some(batch);
            // A DELETE can complete without yielding on a warm disk. Yield so
            // a writer on this thread reaches the gate before the next turn
            // is decided; the turn, not this yield, puts it first. A yielded
            // task is woken only after the scheduler turns the driver without
            // blocking, which fires due timers and reads ready sockets (tokio
            // 1.50 `scheduler/current_thread/mod.rs` 794-798 and 421-434,
            // `multi_thread/worker.rs` 586-590, `time/mod.rs` 254-255).
            tokio::task::yield_now().await;
        };
        // Change numbers are consecutive (each one above the newest), and a
        // trim deletes from the oldest, so this is about the rows left.
        let rows_left = newest - oldest + 1;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        // How long this call kept writes from starting, in all and at most
        // in one go, and what its last batch cost.
        let held_ms = held.as_millis() as u64;
        let longest_batch_ms = longest.as_millis() as u64;
        let (work_ms, commit_ms) = last.map_or((0, 0), |batch| {
            (
                batch.work.as_millis() as u64,
                batch.commit.as_millis() as u64,
            )
        });
        let next_rows = self.inner.trim_plan.lock().unwrap().rows;
        if through >= oldest {
            tracing::info!(
                trimmed,
                batches,
                cuts,
                elapsed_ms,
                held_ms,
                longest_batch_ms,
                work_ms,
                commit_ms,
                next_rows,
                rows_left,
                confirmed,
                "turso.sync trimmed the change log; confirmed rows are left for the next tick"
            );
        } else if trimmed > 0 {
            tracing::debug!(
                trimmed,
                batches,
                cuts,
                elapsed_ms,
                held_ms,
                longest_batch_ms,
                work_ms,
                commit_ms,
                next_rows,
                rows_left,
                confirmed,
                "turso.sync trimmed the change log"
            );
        }
        Ok(trimmed)
    }

    /// Delete every change numbered `upper` or below in one transaction,
    /// timing its DELETE and its COMMIT apart. The DELETE is stopped and
    /// rolled back if it is still running past `plan.cut_after` while a
    /// write waits.
    async fn delete_through(
        &self,
        conn: &Connection,
        upper: i64,
        plan: Plan,
        writers: &impl Writers,
    ) -> Result<Batch, DriverError> {
        let began = Instant::now();
        let tx = conn.begin_immediate().await?;
        let deleting = Instant::now();
        let delete = async {
            #[cfg(feature = "test-support")]
            {
                let work = self.inner.simulated_trim.lock().unwrap().work;
                tokio::time::sleep(work).await;
            }
            // Changes to the change log itself are not captured, so this adds
            // no row of its own (turso_core 0.7.2 `translate/emitter/mod.rs`
            // 1083-1091).
            tx.execute(
                "DELETE FROM turso_cdc WHERE change_id <= ?1",
                crate::driver::params![upper],
            )
            .await
        };
        let a_write_waits_past_the_plan = async {
            tokio::time::sleep(plan.cut_after).await;
            while !writers.waiting() {
                tokio::time::sleep(WRITER_RECHECK).await;
            }
        };
        // Dropping a DELETE mid-statement marks its transaction to roll back
        // (turso_core 0.7.2 `vdbe/mod.rs` 2514-2534), and the ROLLBACK below
        // ends it, releases the write lock and clears that mark
        // (`connection.rs` 2654-2669, 4749-4786).
        let deleted = tokio::select! {
            biased;
            deleted = delete => Some(deleted),
            () = a_write_waits_past_the_plan => None,
        };
        let work = deleting.elapsed();
        let deleted = match deleted {
            Some(Ok(deleted)) => deleted,
            Some(Err(error)) => {
                let _ = tx.rollback().await;
                return Err(error);
            }
            None => {
                tx.rollback().await?;
                return Ok(Batch {
                    planned: plan.rows,
                    deleted: 0,
                    cut: true,
                    work,
                    commit: Duration::ZERO,
                    held: began.elapsed(),
                });
            }
        };
        let committing = Instant::now();
        #[cfg(feature = "test-support")]
        {
            let commit = self.inner.simulated_trim.lock().unwrap().commit;
            tokio::time::sleep(commit).await;
        }
        tx.commit().await?;
        Ok(Batch {
            planned: plan.rows,
            deleted,
            cut: false,
            work,
            commit: committing.elapsed(),
            held: began.elapsed(),
        })
    }

    /// Make every trim batch's DELETE and COMMIT take this much longer while
    /// they hold the write lock, as a drive with cold reads and a slow sync
    /// would. For this crate's tests; never built into the server.
    #[cfg(feature = "test-support")]
    pub fn simulate_slow_trim(&self, work: Duration, commit: Duration) {
        *self.inner.simulated_trim.lock().unwrap() = SimulatedTrim { work, commit };
    }

    /// Rows the next trim batch is planned to delete. For this crate's tests.
    #[cfg(feature = "test-support")]
    pub fn next_trim_rows(&self) -> i64 {
        self.inner.trim_plan.lock().unwrap().rows
    }
}

/// Time added to every trim batch's DELETE and COMMIT (tests only).
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SimulatedTrim {
    work: Duration,
    commit: Duration,
}

/// The oldest and newest change numbers in the log, if it has any rows.
async fn change_ids(conn: &Connection) -> Result<Option<(i64, i64)>, DriverError> {
    // turso 0.7.2 reads one end of the log for a lone MIN or MAX, but scans every
    // row for both in one query (turso_core `translate/optimizer/mod.rs` 662-718).
    let oldest = one_id(conn, "SELECT MIN(change_id) FROM turso_cdc").await?;
    let newest = one_id(conn, "SELECT MAX(change_id) FROM turso_cdc").await?;
    Ok(oldest.zip(newest))
}

async fn one_id(conn: &Connection, sql: &str) -> Result<Option<i64>, DriverError> {
    let mut rows = conn.query(sql, ()).await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let id = row.get::<Option<i64>>(0)?;
    drop(rows);
    Ok(id)
}

/// The plan for the batch after `batch`, when a write gives up after
/// `lock_wait` for the write lock.
///
/// A batch costs its commit plus its rows' work. On a slow drive the commit
/// is mostly a sync that costs the same however many rows it carries: about
/// 170 ms on a Tensorlake drive and 300 ms on tlfs (roll3, 26 Oct 2026).
/// Planned by the whole batch's time, a 50 ms batch can never be met there,
/// so every batch shrank to one row and the trim fell behind the log. So the
/// rows are planned by the DELETE's own rate, to work as long as the commit
/// takes, and at least [`BATCH_TARGET`]:
///
/// - Half of each batch then deletes rows, so trimming takes at most twice
///   the lock time the rows alone would.
/// - This split also keeps the wait it adds to writes lowest. A write that
///   arrives during a batch of length `B` waits `B / 2` on average, and to
///   delete a given number of rows the trim holds the lock a share of the
///   time proportional to `B / (B - commit)`. Their product is smallest at
///   `B = 2 * commit`.
///
/// At most twice the rows of `batch`, so after a warm stretch of the log a
/// cold one costs at most one long batch before the rate shrinks it. Never
/// so many that a write arriving as the batch starts could wait past
/// `lock_wait`, its own limit: twice the DELETE (see [`Plan`]) and the
/// commit, scaled from the last batch as if all of it grew with its rows.
/// That also bounds a commit that does grow with its rows, which would
/// otherwise raise its own target with every batch. A batch stopped for a
/// waiting write had more rows than its time allowed, so the next has half
/// as many in the same time.
fn next_plan(batch: &Batch, lock_wait: Duration) -> Plan {
    let micros = |time: Duration| i128::try_from(time.as_micros()).unwrap_or(i128::MAX).max(1);
    let duration =
        |micros: i128| Duration::from_micros(u64::try_from(micros.max(0)).unwrap_or(u64::MAX));
    if batch.cut {
        return Plan {
            rows: (batch.planned / 2).max(1),
            cut_after: duration(micros(batch.work)).max(BATCH_TARGET),
        };
    }
    let deleted = i128::from(i64::try_from(batch.deleted).unwrap_or(i64::MAX).max(1));
    let target = micros(batch.commit.max(BATCH_TARGET));
    let by_work = deleted * target / micros(batch.work);
    let by_hold = deleted * micros(lock_wait) / micros(batch.work * 2 + batch.commit);
    let rows = by_work.min(by_hold).clamp(1, deleted.saturating_mul(2));
    // The DELETE's expected time at the last batch's rate.
    let expected = micros(batch.work) * rows / deleted;
    Plan {
        rows: i64::try_from(rows).unwrap_or(i64::MAX),
        cut_after: duration(2 * expected).max(BATCH_TARGET),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(deleted: u64, work_ms: u64, commit_ms: u64) -> Batch {
        let work = Duration::from_millis(work_ms);
        let commit = Duration::from_millis(commit_ms);
        Batch {
            planned: i64::try_from(deleted).unwrap(),
            deleted,
            cut: false,
            work,
            commit,
            held: work + commit,
        }
    }

    /// A write's default wait for the write lock (the store's append attempt
    /// timeout).
    const LOCK_WAIT: Duration = Duration::from_secs(5);

    fn rows(batch: &Batch) -> i64 {
        next_plan(batch, LOCK_WAIT).rows
    }

    #[test]
    fn a_slow_commit_grows_the_batch_until_its_rows_work_as_long_as_the_commit() {
        // roll3's canary: one row a batch, ~170 ms of commit. The rows now grow.
        assert_eq!(rows(&batch(1, 5, 170)), 2);
        // 50 rows that took 170 ms of work to a 170 ms commit: keep the size.
        assert_eq!(rows(&batch(50, 170, 170)), 50);
        // 100 rows in 340 ms of work: halve to work as long as the commit.
        assert_eq!(rows(&batch(100, 340, 170)), 50);
    }

    #[test]
    fn a_quick_commit_plans_the_rows_at_the_batch_target() {
        // 25 rows in 1 s of work: about 1 fits in 50 ms.
        assert_eq!(rows(&batch(25, 1000, 1)), 1);
        // 25 rows in 1 ms: 1,250 would fit, but growth is at most double.
        assert_eq!(rows(&batch(25, 1, 1)), 50);
        // Exactly on target keeps the size.
        assert_eq!(rows(&batch(400, 50, 1)), 400);
        // Never below one row, however slow.
        assert_eq!(rows(&batch(1, 60_000, 1)), 1);
    }

    #[test]
    fn no_batch_is_planned_past_a_write_s_lock_wait() {
        // A 3 s commit would plan 3 s of row work. A write arriving as the
        // batch starts could wait twice the DELETE and the commit: 100 rows
        // (1 s of work, cut at 2 s, then 3 s of commit) fit in 5 s.
        assert_eq!(rows(&batch(100, 1000, 3000)), 100);
        // A commit that alone takes past the wait: the batch shrinks to one row.
        assert_eq!(rows(&batch(10, 10, 6000)), 8);
        assert_eq!(rows(&batch(1, 10, 6000)), 1);
    }

    #[test]
    fn a_delete_may_run_twice_its_expected_time_while_a_write_waits() {
        let plan = next_plan(&batch(50, 170, 170), LOCK_WAIT);
        assert_eq!(plan.cut_after, Duration::from_millis(340));
        // Never less than the batch target, however quick the rows.
        let plan = next_plan(&batch(25, 1, 1), LOCK_WAIT);
        assert_eq!(plan.cut_after, BATCH_TARGET);
    }

    #[test]
    fn a_batch_cut_for_a_waiting_write_halves_the_next() {
        let cut = Batch {
            planned: 40,
            deleted: 0,
            cut: true,
            work: Duration::from_millis(400),
            commit: Duration::ZERO,
            held: Duration::from_millis(400),
        };
        let plan = next_plan(&cut, LOCK_WAIT);
        assert_eq!(
            plan,
            Plan {
                rows: 20,
                cut_after: Duration::from_millis(400)
            }
        );
    }
}
