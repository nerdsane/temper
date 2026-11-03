//! One check at a time on a host.
//!
//! Sibling Temper processes on one host share its memory, and each reads the
//! same free memory ([`crate::host_room`]). A spec check sizes its states
//! budget to that, and an install's joint proofs walk in it, so two at once,
//! in two processes, would each take the share meant for one. On a 2 GB
//! sandbox shared with the daemon that holds its data volume, the kernel then
//! kills whichever crosses the limit, and killing that daemon unmounts the
//! volume (15 Oct 2026). So each caller that checks or installs specs
//! (`POST /api/specs/validate-ioa`, and every app install from files, pushed
//! or fetched from a registry) takes an exclusive lock on one fixed file in
//! the system's temporary directory before it reads free memory, and holds it
//! until its work has ended.
//!
//! A caller takes the turn once: a second lock on a new handle in the same
//! process waits for the first, so a function that already holds it must not
//! call one that takes it again.
//!
//! No simulation runs either caller (a registry install fetches over the
//! network), so the real file below is never in a simulated run: the lock
//! exists to coordinate real processes, which a simulation has one of.

use std::fs::{File, OpenOptions}; // determinism-ok: host-wide lock file, HTTP handlers only

use tokio::sync::{Mutex, MutexGuard};

/// The file every Temper process on this host locks for its turn.
const LOCK_FILE: &str = "temper-installs.lock";

// Queue this process's callers before any blocking lock wait. The holder
// needs the blocking pool for verification and joint proofs.
static QUEUE: Mutex<()> = Mutex::const_new(());

/// This host's turn to check or install, held until this guard is dropped.
#[derive(Debug)]
pub struct Turn {
    // Drop the OS lock before admitting the next caller from this process.
    _file: File,
    _queue: MutexGuard<'static, ()>,
}

/// Wait asynchronously for this process's turn, then take the host-wide
/// file lock on a blocking thread. Only one caller per process can wait on
/// that thread, leaving the blocking pool available to the current holder.
/// The operating system releases the lock when its file closes, including
/// when a process dies.
pub async fn take() -> Result<Turn, String> {
    take_at(std::env::temp_dir().join(LOCK_FILE)).await
}

async fn take_at(path: std::path::PathBuf) -> Result<Turn, String> {
    let queue = QUEUE.lock().await;
    // determinism-ok: a blocking thread waits on the host-wide lock
    tokio::task::spawn_blocking(move || {
        // Keep the queue guard inside the worker and its result: cancelling
        // the caller cannot admit another blocking wait before this ends.
        let _queue = queue;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| {
                format!(
                    "the host's check lock '{}' could not be opened: {error}",
                    path.display()
                )
            })?;
        file.lock().map_err(|error| {
            format!(
                "the host's check lock '{}' could not be taken: {error}",
                path.display()
            )
        })?;
        Ok(Turn {
            _file: file,
            _queue,
        })
    })
    .await
    .map_err(|error| format!("the host's check lock's thread failed: {error}"))?
}

#[cfg(test)]
#[path = "host_turn_test.rs"]
mod tests;
