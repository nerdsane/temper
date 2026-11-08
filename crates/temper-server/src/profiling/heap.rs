//! Heap profiles: jemalloc's sampled live allocations, as pprof.
//!
//! jemalloc records a stack for about one allocation in every 2^`lg_prof_sample`
//! bytes while `prof.active` is on, and a dump scales the samples back up to
//! the bytes they stand for. A dump is a snapshot of what is live now, so it
//! needs no capture window. Available where the binary's jemalloc was built
//! with profiling and started with `prof:true`; that is Linux only.

use pprof::protos::Profile;

/// Why a heap profile could not be taken.
#[derive(Debug)]
pub(super) enum HeapError {
    /// This binary's allocator has no profile to give.
    Unavailable(&'static str),
    /// The dump or its conversion failed.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Failed(String),
}

/// What a heap sample counts, as Datadog's native profiler names it.
#[cfg(target_os = "linux")]
const HEAP_SAMPLE_TYPES: [(&str, &str); 1] = [("inuse-space", "bytes")];

/// Whether this process can give heap profiles.
pub(super) fn available() -> bool {
    imp::available()
}

/// A pprof snapshot of the sampled live heap.
pub(super) async fn capture_heap() -> Result<Vec<u8>, HeapError> {
    imp::capture_heap().await
}

/// The frame jemalloc's sampling hook is entered through. It and every frame
/// above it are the hook; the frame below it is the allocator call that ran
/// the hook (`malloc`, `rallocx` and the like).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SAMPLING_HOOK: &str = "prof_tctx_create";

/// Start each sampled stack at the code that allocated.
///
/// jemalloc records a stack from inside its own sampling hook, so every
/// sample leads with the hook's frames and the allocator entry that called
/// it. Left in, every byte is charged to the hook in a flat view. Samples
/// without the hook stay as they are.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn start_at_the_caller(profile: &mut Profile) {
    let names = |location_id: u64| -> Vec<&str> {
        let Some(location) = profile.location.iter().find(|l| l.id == location_id) else {
            return Vec::new();
        };
        location
            .line
            .iter()
            .filter_map(|line| profile.function.iter().find(|f| f.id == line.function_id))
            .filter_map(|f| profile.string_table.get(f.name as usize))
            .map(String::as_str)
            .collect()
    };
    let cuts: Vec<usize> = profile
        .sample
        .iter()
        .map(|sample| {
            sample
                .location_id
                .iter()
                .position(|id| names(*id).iter().any(|n| n.ends_with(SAMPLING_HOOK)))
                // The hook, the frames above it and the allocator entry below it.
                .map_or(0, |hook| (hook + 2).min(sample.location_id.len()))
        })
        .collect();
    for (sample, cut) in profile.sample.iter_mut().zip(cuts) {
        sample.location_id.drain(..cut);
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io::Read;

    use pprof::protos::{Message, Profile};

    use super::super::capture::{encode, name_sample_types};
    use super::{HEAP_SAMPLE_TYPES, HeapError};

    pub(super) fn available() -> bool {
        jemalloc_pprof::PROF_CTL.is_some()
    }

    pub(super) async fn capture_heap() -> Result<Vec<u8>, HeapError> {
        let ctl = jemalloc_pprof::PROF_CTL
            .as_ref()
            .ok_or(HeapError::Unavailable(
                "jemalloc was not started with prof:true",
            ))?;
        let mut ctl = ctl.lock().await;
        if !ctl.activated() {
            return Err(HeapError::Unavailable("jemalloc profiling is not active"));
        }
        let gzipped = ctl
            .dump_pprof()
            .map_err(|e| HeapError::Failed(format!("dump: {e}")))?;
        drop(ctl);
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(gzipped.as_slice())
            .read_to_end(&mut raw)
            .map_err(|e| HeapError::Failed(format!("gunzip: {e}")))?;
        let mut profile = Profile::decode(raw.as_slice())
            .map_err(|e| HeapError::Failed(format!("decode: {e}")))?;
        name_sample_types(&mut profile, &HEAP_SAMPLE_TYPES);
        super::start_at_the_caller(&mut profile);
        encode(&profile).map_err(HeapError::Failed)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::HeapError;

    pub(super) fn available() -> bool {
        false
    }

    pub(super) async fn capture_heap() -> Result<Vec<u8>, HeapError> {
        Err(HeapError::Unavailable(
            "heap profiles need jemalloc's profiler, built on Linux only",
        ))
    }
}
