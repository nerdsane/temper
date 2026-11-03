//! The room a local store's disk keeps, and whether the store takes writes.
//!
//! Every append goes through [`BoxedEventStore`], so it is where a disk under
//! the floor refuses an append before anything is written, and where each
//! append's outcome is kept for `/healthz` (`crate::write_health`). The one
//! other write every move makes, its trajectory row, is refused there too
//! ([`RoomCheckedTrajectories`]).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use temper_runtime::persistence::PersistenceError;

use super::{BoxedEventStore, TrajectorySink};
use crate::state::trajectory::TrajectoryEntry;

/// How much room the disk under a local store keeps free: appends are refused
/// while it has less than this available to an ordinary process (statvfs
/// `f_bavail`).
///
/// Why 160 MiB, from the full-disk tests on staging (26 Oct 2026). On a
/// Tensorlake volume a full disk is not a clean failure: the volume's daemon
/// (tlfs) stops copying the volume off the sandbox for good while writes still
/// answer success, so what is written after that is lost; and under a database
/// its own store fails ("Poisoned"), so every later read and write answers EIO
/// until the volume is mounted again. tlfs holds up to 160 MiB of writes it has
/// already answered in its pipeline pools (records 64 MiB, segments 96 MiB, from
/// its "TLFS pipeline pools" log line) and puts them in its store, on the same
/// disk, after the writer is answered. With writers stopped at this floor the
/// disk went on filling by 49 to 55 MB, with one writer and with four at about
/// 11 MB/s; the least available was 110 MB, root's free space never fell under
/// 1068 MB, nothing failed, and the writers went on by themselves once space
/// was freed. The 5% ext4 keeps for root lies under this floor as tlfs's own
/// room: it runs as root.
pub const STORAGE_FLOOR_BYTES: u64 = 160 << 20;

/// Reads what the disk holding a directory has available, in bytes.
type ReadAvailable = Arc<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

/// The disk a local store writes, and how its room is read.
#[derive(Clone)]
pub struct StorageRoom {
    dir: PathBuf,
    available: ReadAvailable,
}

impl StorageRoom {
    /// The disk holding `dir`, read with statvfs.
    pub fn on_disk(dir: impl Into<PathBuf>) -> Self {
        Self::read_with(dir, available_bytes)
    }

    /// The disk holding `dir`, read with `available`: a disk a test sets.
    pub fn read_with(
        dir: impl Into<PathBuf>,
        available: impl Fn(&Path) -> io::Result<u64> + Send + Sync + 'static,
    ) -> Self {
        Self {
            dir: dir.into(),
            available: Arc::new(available),
        }
    }

    /// The directory whose disk is read.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Refuse while the disk has less than the floor available. A disk that
    /// cannot be read refuses nothing: the write itself then says what is wrong.
    fn check(&self) -> Result<(), PersistenceError> {
        match (self.available)(&self.dir) {
            Ok(available_bytes) if available_bytes < STORAGE_FLOOR_BYTES => {
                Err(PersistenceError::StorageFull {
                    dir: self.dir.display().to_string(),
                    available_bytes,
                    floor_bytes: STORAGE_FLOOR_BYTES,
                })
            }
            _ => Ok(()),
        }
    }
}

/// Whether the store takes writes, as its appends show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteHealth {
    /// No append has failed since the last one that landed.
    Writing,
    /// Appends are refused because the disk is under the floor: the refusal.
    Refusing(String),
    /// The last append failed in the store and none has landed since: why.
    Failing(String),
}

/// The write health every clone of one [`BoxedEventStore`] shares.
#[derive(Clone)]
pub(super) struct HealthCell(Arc<Mutex<WriteHealth>>);

impl Default for HealthCell {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(WriteHealth::Writing)))
    }
}

impl HealthCell {
    fn get(&self) -> WriteHealth {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keep `next`, logging once when the store starts or stops taking writes.
    /// Only a landed append ends a failure: a refusal for room on a store that
    /// failed keeps it failing, so freeing the disk does not hide a store that
    /// still needs a restart.
    fn set(&self, next: WriteHealth) {
        let mut health = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match (&*health, &next) {
            (WriteHealth::Failing(_), WriteHealth::Refusing(_)) => return,
            (WriteHealth::Writing, WriteHealth::Writing)
            | (WriteHealth::Refusing(_), WriteHealth::Refusing(_))
            | (WriteHealth::Failing(_), WriteHealth::Failing(_)) => {}
            (_, WriteHealth::Writing) => tracing::info!("the store takes writes again"),
            (_, WriteHealth::Refusing(error)) => {
                tracing::warn!(%error, "storage full: appends refused until the disk has room")
            }
            (_, WriteHealth::Failing(error)) => tracing::error!(
                %error,
                "the store failed an append: /healthz answers 503 until one lands"
            ),
        }
        *health = next;
    }
}

impl BoxedEventStore {
    /// Refuse appends while the disk holding `room` has less than
    /// [`STORAGE_FLOOR_BYTES`] available. Every append checks, so writes go on
    /// by themselves once the disk has room again.
    pub fn with_storage_room(mut self, room: StorageRoom) -> Self {
        self.room = Some(room);
        self
    }

    /// Whether the store takes writes. A refusal for room is checked again
    /// here, so it ends when the disk has room, before the next append.
    pub fn write_health(&self) -> WriteHealth {
        let health = self.health.get();
        if matches!(health, WriteHealth::Refusing(_)) && self.check_room().is_ok() {
            self.health.set(WriteHealth::Writing);
            return WriteHealth::Writing;
        }
        health
    }

    /// Refuse an append, before anything is written, while the disk is under
    /// the floor. A create checks it before its actor starts.
    pub(crate) fn check_room(&self) -> Result<(), PersistenceError> {
        let Some(room) = &self.room else {
            return Ok(());
        };
        room.check()
            .inspect_err(|error| self.health.set(WriteHealth::Refusing(error.to_string())))
    }

    /// Keep what one append did for `/healthz`, and hand its result back. A
    /// conflict, an event too large or one that would not serialize says
    /// nothing about whether the store takes writes.
    pub(super) fn record_append<T>(
        &self,
        result: Result<T, PersistenceError>,
    ) -> Result<T, PersistenceError> {
        match &result {
            Ok(_) => self.health.set(WriteHealth::Writing),
            Err(error @ PersistenceError::StorageFull { .. }) => {
                self.health.set(WriteHealth::Refusing(error.to_string()))
            }
            Err(error @ (PersistenceError::Storage(_) | PersistenceError::OutcomeUnknown(_))) => {
                self.health.set(WriteHealth::Failing(error.to_string()))
            }
            Err(
                PersistenceError::ConcurrencyViolation { .. }
                | PersistenceError::Serialization(_)
                | PersistenceError::EventTooLarge { .. },
            ) => {}
        }
        result
    }
}

/// A trajectory sink that writes no row while the store's disk is under the
/// floor. A refused move still records its trajectory row, so a client that
/// keeps retrying would spend the room the floor keeps (on staging the disk
/// went on losing about 0.2 MB/s while every append was refused). The outbox
/// counts each row refused here as a capture loss and marks its session, as it
/// does for any failed write.
pub(super) struct RoomCheckedTrajectories {
    pub(super) inner: Arc<dyn TrajectorySink>,
    pub(super) room: StorageRoom,
}

#[async_trait::async_trait]
impl TrajectorySink for RoomCheckedTrajectories {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        self.room.check().map_err(|error| error.to_string())?;
        self.inner.persist_trajectory_entry(entry).await
    }
}

/// What the disk holding `dir` has available to an ordinary process, in bytes:
/// statvfs `f_bavail` blocks of `f_frsize` bytes.
#[cfg(unix)]
fn available_bytes(dir: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string that outlives the call, and
    // statvfs fills `stat` whenever it answers 0.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs answered 0, so it filled `stat`.
    let stat = unsafe { stat.assume_init() };
    // The field types differ by platform (u32 or u64), so both widen to u128.
    let bytes = (stat.f_bavail as u128) * (stat.f_frsize as u128);
    Ok(u64::try_from(bytes).unwrap_or(u64::MAX))
}

/// A disk this platform cannot read refuses nothing.
#[cfg(not(unix))]
fn available_bytes(_dir: &Path) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "statvfs is not available on this platform",
    ))
}

#[cfg(test)]
#[path = "write_room_test.rs"]
mod tests;
