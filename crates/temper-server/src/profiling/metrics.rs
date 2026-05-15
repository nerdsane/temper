use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Gauge, Histogram};

pub(super) struct ProfilingMetrics {
    pub(super) profiles_uploaded: Counter<u64>,
    pub(super) upload_errors: Counter<u64>,
    pub(super) captures_started: Counter<u64>,
    pub(super) captures_completed: Counter<u64>,
    pub(super) capture_errors: Counter<u64>,
    pub(super) capture_duration_ms: Histogram<f64>,
    pub(super) last_capture_unix_seconds: Gauge<u64>,
    config: Gauge<u64>,
    continuous_interval_seconds: Gauge<u64>,
    continuous_window_seconds: Gauge<u64>,
}

pub(super) fn profiling_metrics() -> &'static ProfilingMetrics {
    static M: OnceLock<ProfilingMetrics> = OnceLock::new();
    M.get_or_init(|| {
        let meter = global::meter("temper.profiling");
        ProfilingMetrics {
            profiles_uploaded: meter
                .u64_counter("datadog.profiling.rust.profiles_uploaded")
                .with_description(
                    "Profiles successfully pushed to the Datadog Agent intake endpoint.",
                )
                .build(),
            upload_errors: meter
                .u64_counter("datadog.profiling.rust.upload_errors")
                .with_description("Profile upload attempts that failed to reach the Datadog Agent.")
                .build(),
            captures_started: meter
                .u64_counter("temper_profiling_capture_started_total")
                .with_description("Profile capture attempts started by Temper.")
                .build(),
            captures_completed: meter
                .u64_counter("temper_profiling_capture_completed_total")
                .with_description("Profile captures that produced a pprof payload.")
                .build(),
            capture_errors: meter
                .u64_counter("temper_profiling_capture_errors_total")
                .with_description("Profile captures that failed before producing a pprof payload.")
                .build(),
            capture_duration_ms: meter
                .f64_histogram("temper_profiling_capture_duration_ms")
                .with_description("Wall-clock time spent collecting and encoding a profile.")
                .build(),
            last_capture_unix_seconds: meter
                .u64_gauge("temper_profiling_last_capture_unix_seconds")
                .with_description("Unix timestamp for the last successfully encoded profile.")
                .build(),
            config: meter
                .u64_gauge("temper_profiling_config")
                .with_description("Profiler runtime config flags as 0/1 gauges.")
                .build(),
            continuous_interval_seconds: meter
                .u64_gauge("temper_profiling_continuous_interval_seconds")
                .with_description("Configured interval between continuous profile captures.")
                .build(),
            continuous_window_seconds: meter
                .u64_gauge("temper_profiling_continuous_window_seconds")
                .with_description("Configured duration of each continuous profile capture.")
                .build(),
        }
    })
}

fn bool_as_gauge(value: bool) -> u64 {
    if value { 1 } else { 0 }
}

pub(super) fn current_unix_seconds() -> u64 {
    SystemTime::now() // determinism-ok: observability timestamp
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Emit profiler configuration gauges so Datadog can tell the difference
/// between "no profiles because disabled" and "no profiles because broken".
pub fn record_profiling_config() {
    let metrics = profiling_metrics();
    metrics.config.record(
        bool_as_gauge(super::profiling_enabled()),
        &[KeyValue::new("setting", "enabled")],
    );
    metrics.config.record(
        bool_as_gauge(super::auto_upload_enabled()),
        &[KeyValue::new("setting", "auto_upload")],
    );
    metrics.config.record(
        bool_as_gauge(super::continuous_enabled()),
        &[KeyValue::new("setting", "continuous")],
    );
    metrics
        .continuous_interval_seconds
        .record(super::continuous_interval_seconds(), &[]);
    metrics
        .continuous_window_seconds
        .record(super::continuous_window_seconds(), &[]);
}

pub(super) fn profile_attrs(profile_type: &str, mode: &str) -> [KeyValue; 2] {
    [
        KeyValue::new("profile_type", profile_type.to_string()),
        KeyValue::new("mode", mode.to_string()),
    ]
}

pub(super) fn profile_stage_attrs(profile_type: &str, mode: &str, stage: &str) -> [KeyValue; 3] {
    [
        KeyValue::new("profile_type", profile_type.to_string()),
        KeyValue::new("mode", mode.to_string()),
        KeyValue::new("stage", stage.to_string()),
    ]
}

pub(super) fn record_capture_error(profile_type: &str, mode: &str, stage: &str) {
    profiling_metrics()
        .capture_errors
        .add(1, &profile_stage_attrs(profile_type, mode, stage));
}
