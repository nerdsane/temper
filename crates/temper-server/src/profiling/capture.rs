//! Capture a CPU profile and encode it as pprof in Datadog's native profile
//! vocabulary.

use std::time::{Duration, Instant};

use pprof::protos::{Message, Profile, ValueType};
use tokio::time::sleep as sleep_for_profile_capture; // determinism-ok: bounded profiler capture window

use super::metrics::{profile_attrs, record_capture_done, record_capture_error};
use super::symbols;

/// What a sample counts in a CPU profile, as Datadog's native profiler names
/// it: samples taken, and the CPU time they stand for.
const CPU_SAMPLE_TYPES: [(&str, &str); 2] = [("cpu-samples", "count"), ("cpu-time", "nanoseconds")];

/// Sample the process's CPU for `seconds` at `frequency` Hz; pprof bytes.
pub(super) async fn capture_cpu(
    seconds: u64,
    frequency: i32,
    profile_type: &str,
    mode: &str,
) -> Result<Vec<u8>, String> {
    debug_assert!(seconds > 0 && frequency > 0, "a capture samples something");
    let started = Instant::now(); // determinism-ok: observability duration only
    super::metrics::profiling_metrics()
        .captures_started
        .add(1, &profile_attrs(profile_type, mode));

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(frequency)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .map_err(|e| fail(profile_type, mode, "profiler_start", e))?;

    // The timer samples in the background while this task yields.
    sleep_for_profile_capture(Duration::from_secs(seconds)).await;

    let unresolved = guard
        .report()
        .build_unresolved()
        .map_err(|e| fail(profile_type, mode, "report_build", e))?;
    let mut profile = symbols::resolve(unresolved)
        .pprof()
        .map_err(|e| fail(profile_type, mode, "pprof_conversion", e))?;
    name_sample_types(&mut profile, &CPU_SAMPLE_TYPES);
    profile.period_type = profile.sample_type.get(1).cloned();

    let body = encode(&profile).map_err(|e| fail(profile_type, mode, "pprof_serialize", e))?;
    record_capture_done(profile_type, mode, started);
    Ok(body)
}

/// Encode `profile` as pprof protobuf bytes.
pub(super) fn encode(profile: &Profile) -> Result<Vec<u8>, String> {
    let mut body = Vec::with_capacity(profile.encoded_len());
    profile.encode(&mut body).map_err(|e| e.to_string())?;
    debug_assert!(!body.is_empty(), "a profile encodes to something");
    Ok(body)
}

/// Give `profile`'s sample types the given names, in order. New strings are
/// appended so names that functions share keep their meaning.
pub(super) fn name_sample_types(profile: &mut Profile, names: &[(&str, &str)]) {
    assert_eq!(
        profile.sample_type.len(),
        names.len(),
        "one name per sample type"
    );
    let mut index = |text: &str| -> i64 {
        match profile.string_table.iter().position(|s| s == text) {
            Some(at) => at as i64,
            None => {
                profile.string_table.push(text.to_string());
                (profile.string_table.len() - 1) as i64
            }
        }
    };
    let named: Vec<ValueType> = names
        .iter()
        .map(|(ty, unit)| ValueType {
            ty: index(ty),
            unit: index(unit),
        })
        .collect();
    profile.sample_type = named;
}

fn fail(profile_type: &str, mode: &str, stage: &str, error: impl std::fmt::Display) -> String {
    record_capture_error(profile_type, mode, stage);
    format!("{stage}: {error}")
}
