//! The platform IO, counting every read from the files it opens.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{TursoEventStore, sqlite_test_url};
use turso::core::io::{FileId, FileSyncType};
use turso::core::{
    Buffer, Clock, Completion, File, IO, MemoryIO, MonotonicInstant, OpenFlags, PlatformIO, Result,
    WallClockInstant,
};

/// [`PlatformIO`] with a count of the reads made through it.
pub(super) struct CountingIo {
    inner: Arc<dyn IO>,
    reads: Arc<AtomicUsize>,
}

impl CountingIo {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(PlatformIO::new().expect("platform io")),
            reads: Arc::default(),
        })
    }

    /// Reads since the last [`Self::reset`]; one read is one page or WAL frame.
    pub(super) fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    pub(super) fn reset(&self) {
        self.reads.store(0, Ordering::SeqCst);
    }
}

/// The database at `url`, its writer dropped, opened again as a restart
/// would: from a cold cache, through counted reads. It is a copy at a new
/// path named after `name`: the engine shares one open database, and its
/// cache, per path within a process.
pub(super) async fn reopen_cold(url: &str, name: &str) -> (TursoEventStore, Arc<CountingIo>) {
    let from = url.strip_prefix("file:").unwrap();
    let copy = sqlite_test_url(name);
    let copy = copy.strip_prefix("file:").unwrap();
    for suffix in ["", "-wal"] {
        if std::path::Path::new(&format!("{from}{suffix}")).exists() {
            std::fs::copy(format!("{from}{suffix}"), format!("{copy}{suffix}")).unwrap();
        }
    }
    let io = CountingIo::new();
    let db = crate::driver::Database::local_with_io(copy, io.clone())
        .await
        .unwrap();
    let store = TursoEventStore::open(db, false, None).await.unwrap();
    (store, io)
}

struct CountingFile {
    inner: Arc<dyn File>,
    reads: Arc<AtomicUsize>,
}

impl File for CountingFile {
    fn lock_file(&self, exclusive: bool) -> Result<()> {
        self.inner.lock_file(exclusive)
    }
    fn unlock_file(&self) -> Result<()> {
        self.inner.unlock_file()
    }
    fn pread(&self, pos: u64, c: Completion) -> Result<Completion> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.pread(pos, c)
    }
    fn pwrite(&self, pos: u64, buffer: Arc<Buffer>, c: Completion) -> Result<Completion> {
        self.inner.pwrite(pos, buffer, c)
    }
    fn pwritev(&self, pos: u64, buffers: Vec<Arc<Buffer>>, c: Completion) -> Result<Completion> {
        self.inner.pwritev(pos, buffers, c)
    }
    fn sync(&self, c: Completion, sync_type: FileSyncType) -> Result<Completion> {
        self.inner.sync(c, sync_type)
    }
    fn size(&self) -> Result<u64> {
        self.inner.size()
    }
    fn truncate(&self, len: u64, c: Completion) -> Result<Completion> {
        self.inner.truncate(len, c)
    }
}

impl Clock for CountingIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.inner.current_time_monotonic()
    }
    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.inner.current_time_wall_clock()
    }
}

impl IO for CountingIo {
    fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
        Ok(Arc::new(CountingFile {
            inner: self.inner.open_file(path, flags, direct)?,
            reads: self.reads.clone(),
        }))
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn step(&self) -> Result<()> {
        self.inner.step()
    }
    fn cancel(&self, c: &[Completion]) -> Result<()> {
        self.inner.cancel(c)
    }
    fn drain_completions(&self, completions: &[Completion]) -> Result<()> {
        self.inner.drain_completions(completions)
    }
    fn wait_for_completion(&self, c: Completion) -> Result<()> {
        self.inner.wait_for_completion(c)
    }
    fn generate_random_number(&self) -> i64 {
        self.inner.generate_random_number()
    }
    fn fill_bytes(&self, dest: &mut [u8]) {
        self.inner.fill_bytes(dest)
    }
    fn get_memory_io(&self) -> Arc<MemoryIO> {
        self.inner.get_memory_io()
    }
    fn register_fixed_buffer(&self, ptr: NonNull<u8>, len: usize) -> Result<u32> {
        self.inner.register_fixed_buffer(ptr, len)
    }
    fn yield_now(&self) {
        self.inner.yield_now()
    }
    fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration)
    }
    fn file_id(&self, path: &str) -> Result<FileId> {
        self.inner.file_id(path)
    }
}
