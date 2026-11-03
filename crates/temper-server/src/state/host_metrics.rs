//! Host readings sampled with the runtime metrics: the whole machine's memory,
//! the used share of chosen disks, and the bytes chosen directory trees take.
//!
//! They describe the machine the server runs on, not the server process, so
//! they cover every other program sharing it. A reading the platform does not
//! offer is left out, never reported as zero: memory off Linux, disks off Unix.

use std::ffi::OsStr;
use std::fs::Metadata; // determinism-ok: host readings measure the real filesystem
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Gauge, Meter};

/// A pattern segment that stands for every entry of its directory.
const ANY_SEGMENT: &str = "*";

/// Why a directory walk stopped without a size.
#[derive(Debug, thiserror::Error)]
enum WalkStop {
    #[error("ran past its time limit of {0:?}")]
    TimedOut(Duration),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What the host readings read; taken from the environment at startup.
pub(super) struct HostMetricSettings {
    /// Disks whose used share is reported, one series per path.
    pub(super) disk_paths: Vec<String>,
    /// Directory patterns whose trees are measured, one series per pattern.
    /// A segment that is exactly `*` matches every entry of its directory.
    pub(super) dir_patterns: Vec<String>,
    /// How long the walk for one pattern may take before it gives up.
    pub(super) dir_time_limit: Duration,
}

/// The host gauges, and the directory walk they share with its thread.
pub(super) struct HostMetricInstruments {
    memory_used_ratio: Gauge<f64>,
    memory_total_bytes: Gauge<u64>,
    disk_used_ratio: Gauge<f64>,
    disk_paths: Vec<String>,
    dir_walk: Arc<DirWalk>,
}

/// What a directory walk needs on its blocking thread.
struct DirWalk {
    dir_bytes: Gauge<u64>,
    patterns: Vec<String>,
    time_limit: Duration,
    /// Set while a walk runs. A walk can sit in one filesystem call for as long
    /// as the filesystem takes; starting another each tick would pile up
    /// blocking threads behind it, so a tick skips the walk while one runs.
    running: AtomicBool,
}

/// Clears `DirWalk::running` when the walk ends, panicked or not.
struct WalkRunning(Arc<DirWalk>);

impl Drop for WalkRunning {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
    }
}

/// The machine's memory from `/proc/meminfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostMemory {
    total_bytes: u64,
    available_bytes: u64,
}

impl HostMemory {
    /// The share in use: what is not available for new work without swapping.
    fn used_ratio(self) -> f64 {
        debug_assert!(self.total_bytes > 0);
        self.total_bytes.saturating_sub(self.available_bytes) as f64 / self.total_bytes as f64
    }
}

/// When a walk must give up.
#[derive(Debug, Clone, Copy)]
struct WalkDeadline {
    /// None when the limit is too far off to represent; it never passes.
    at: Option<Instant>,
    time_limit: Duration,
}

impl HostMetricInstruments {
    /// Build the host gauges on `meter`.
    pub(super) fn new(meter: &Meter, settings: HostMetricSettings) -> Self {
        Self {
            memory_used_ratio: meter
                .f64_gauge("temper_host_memory_used_ratio")
                .with_description(
                    "Share of the whole machine's memory in use: (MemTotal - MemAvailable) / MemTotal.",
                )
                .build(),
            memory_total_bytes: meter
                .u64_gauge("temper_host_memory_total_bytes")
                .with_unit("By")
                .with_description("The whole machine's memory (MemTotal).")
                .build(),
            disk_used_ratio: meter
                .f64_gauge("temper_host_disk_used_ratio")
                .with_description(
                    "Used share of the filesystem holding `path`, as df counts it: used / (used + available).",
                )
                .build(),
            disk_paths: settings.disk_paths,
            dir_walk: Arc::new(DirWalk {
                dir_bytes: meter
                    .u64_gauge("temper_host_dir_bytes")
                    .with_unit("By")
                    .with_description(
                        "Bytes on disk of every directory tree matching the pattern `path`, as du counts them.",
                    )
                    .build(),
                patterns: settings.dir_patterns,
                time_limit: settings.dir_time_limit,
                running: AtomicBool::new(false),
            }),
        }
    }

    /// Record memory and disks now, and start the directory walk, which
    /// records its sizes when it finishes; this call never waits for it.
    pub(super) fn record(&self) {
        if let Some(memory) = read_host_memory() {
            self.memory_total_bytes.record(memory.total_bytes, &[]);
            self.memory_used_ratio.record(memory.used_ratio(), &[]);
        }
        for path in &self.disk_paths {
            match read_disk_used_ratio(Path::new(path)) {
                Ok(Some(ratio)) => self
                    .disk_used_ratio
                    .record(ratio, &[KeyValue::new("path", path.clone())]),
                Ok(None) => {}
                Err(error) => tracing::warn!(path = %path, %error, "disk usage not reported"),
            }
        }
        self.start_dir_walk();
    }

    fn start_dir_walk(&self) {
        let walk = &self.dir_walk;
        if walk.patterns.is_empty() || walk.running.swap(true, Ordering::AcqRel) {
            return;
        }
        debug_assert!(
            walk.running.load(Ordering::Acquire),
            "this tick owns the walk until it ends"
        );
        let running = WalkRunning(Arc::clone(walk));
        // determinism-ok: host metrics read the real filesystem, off the async runtime
        tokio::task::spawn_blocking(move || running.0.run());
    }
}

impl DirWalk {
    /// Measure every pattern and record each size; runs on a blocking thread.
    fn run(&self) {
        for pattern in &self.patterns {
            match pattern_bytes(pattern, self.time_limit) {
                Ok(Some(bytes)) => self
                    .dir_bytes
                    .record(bytes, &[KeyValue::new("path", pattern.clone())]),
                Ok(None) => {}
                Err(stop) => {
                    tracing::warn!(path = %pattern, error = %stop, "directory size not reported");
                }
            }
        }
    }
}

impl WalkDeadline {
    fn after(time_limit: Duration) -> Self {
        let now = Instant::now(); // determinism-ok: a walk's time limit is real elapsed time
        Self {
            at: now.checked_add(time_limit),
            time_limit,
        }
    }

    /// Err(TimedOut) once the deadline has passed.
    fn check(self) -> Result<(), WalkStop> {
        let now = Instant::now(); // determinism-ok: a walk's time limit is real elapsed time
        match self.at {
            Some(at) if now >= at => Err(WalkStop::TimedOut(self.time_limit)),
            _ => Ok(()),
        }
    }
}

// SECTION: memory

/// None where there is no `/proc/meminfo`, which is everywhere but Linux.
fn read_host_memory() -> Option<HostMemory> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?; // determinism-ok: procfs read for observability
    parse_meminfo(&meminfo)
}

/// MemTotal and MemAvailable from the text of `/proc/meminfo`, whose values
/// are in kibibytes. None when either is missing (kernels before 3.14 have no
/// MemAvailable) or the total is zero.
fn parse_meminfo(text: &str) -> Option<HostMemory> {
    let mut total_kib = None;
    let mut available_kib = None;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let kib = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok());
        match name {
            "MemTotal" => total_kib = kib,
            "MemAvailable" => available_kib = kib,
            _ => {}
        }
    }
    let total_bytes = total_kib?.checked_mul(1024)?;
    let available_bytes = available_kib?.checked_mul(1024)?;
    (total_bytes > 0).then_some(HostMemory {
        total_bytes,
        available_bytes,
    })
}

// SECTION: disks

/// The used share of the filesystem holding `path`. Ok(None) when it has no
/// usable blocks (a pseudo-filesystem such as /proc).
#[cfg(unix)]
fn read_disk_used_ratio(path: &Path) -> std::io::Result<Option<f64>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs is plain data, for which all zeroes is a valid value.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // JUSTIFICATION: std has no call for a filesystem's usage, and libc is
    // already a dependency.
    // SAFETY: c_path is NUL-terminated and outlives the call; stat is a valid,
    // writable statvfs that the call only writes into.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(df_used_ratio(
        stat.f_blocks as f64,
        stat.f_bfree as f64,
        stat.f_bavail as f64,
    ))
}

#[cfg(not(unix))]
fn read_disk_used_ratio(_path: &Path) -> std::io::Result<Option<f64>> {
    Ok(None)
}

/// Used space as df counts it: used / (used + available to an ordinary
/// process). Blocks reserved for root count as neither, so a disk reads 100%
/// when ordinary programs can no longer write, before every block is gone.
#[cfg(unix)]
fn df_used_ratio(total_blocks: f64, free_blocks: f64, available_blocks: f64) -> Option<f64> {
    let used = total_blocks - free_blocks;
    let usable = used + available_blocks;
    (usable > 0.0).then(|| used / usable)
}

// SECTION: directory trees

/// Bytes on disk of every tree matching `pattern`, added up. Ok(None) when
/// nothing matches: there is nothing to measure, which is not zero bytes.
fn pattern_bytes(pattern: &str, time_limit: Duration) -> Result<Option<u64>, WalkStop> {
    let deadline = WalkDeadline::after(time_limit);
    let roots = expand_pattern(Path::new(pattern), deadline)?;
    if roots.is_empty() {
        return Ok(None);
    }
    let mut total: u64 = 0;
    for root in &roots {
        total = total.saturating_add(tree_bytes(root, deadline)?);
    }
    Ok(Some(total))
}

/// Every existing path `pattern` names, sorted, with each `*` segment
/// replaced by each entry of the directory before it.
fn expand_pattern(pattern: &Path, deadline: WalkDeadline) -> Result<Vec<PathBuf>, WalkStop> {
    let any = Component::Normal(OsStr::new(ANY_SEGMENT));
    let mut candidates = vec![PathBuf::new()];
    for component in pattern.components() {
        if component != any {
            for candidate in &mut candidates {
                candidate.push(component);
            }
            continue;
        }
        let mut expanded = Vec::new();
        for parent in &candidates {
            deadline.check()?;
            let listing = std::fs::read_dir(parent); // determinism-ok: host readings
            let entries = match listing {
                Ok(entries) => entries,
                Err(error) if names_nothing(&error) => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                expanded.push(entry?.path());
            }
        }
        candidates = expanded;
    }
    let mut existing = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let found = std::fs::metadata(&candidate); // determinism-ok: host readings
        match found {
            Ok(_) => existing.push(candidate),
            Err(error) if names_nothing(&error) => {}
            Err(error) => return Err(error.into()),
        }
    }
    existing.sort();
    Ok(existing)
}

/// Bytes on disk of `root` and everything below it, as du counts them: the
/// root is followed if it is a symbolic link, links below it are not.
fn tree_bytes(root: &Path, deadline: WalkDeadline) -> Result<u64, WalkStop> {
    deadline.check()?;
    let root_metadata = std::fs::metadata(root)?; // determinism-ok: host readings
    let mut total = disk_bytes(&root_metadata);
    if !root_metadata.is_dir() {
        return Ok(total);
    }
    // An explicit stack, so a deep tree cannot overflow the thread's stack.
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        deadline.check()?;
        let listing = std::fs::read_dir(&dir); // determinism-ok: host readings
        let entries = match listing {
            Ok(entries) => entries,
            // Removed while the walk ran: it no longer takes any space.
            Err(error) if names_nothing(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            deadline.check()?;
            let entry = entry?;
            // DirEntry::metadata does not follow symbolic links.
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if names_nothing(&error) => continue,
                Err(error) => return Err(error.into()),
            };
            total = total.saturating_add(disk_bytes(&metadata));
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(total)
}

/// The path does not exist, or a part of it is not a directory.
fn names_nothing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Bytes an entry takes on disk. POSIX counts st_blocks in 512-byte units,
/// whatever the filesystem's own block size.
#[cfg(unix)]
fn disk_bytes(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

/// Off Unix, std offers only a file's length, not what it takes on disk.
#[cfg(not(unix))]
fn disk_bytes(metadata: &Metadata) -> u64 {
    metadata.len()
}
