//! Profiles of the Temper runtime (ADR-0055): CPU from a sampling timer and
//! heap from jemalloc's sampling allocator profile, both as pprof.
//!
//! Operators capture one on demand at `/_admin/profile/{cpu,wall,heap}`. With
//! `TEMPER_PROFILING_CONTINUOUS=true` a background loop captures a CPU window
//! and a heap snapshot every interval and uploads both to a Datadog profile
//! intake, so profiles stay fresh without anyone catching the hot window.
//!
//! CPU sampling uses `setitimer(ITIMER_PROF)`, not `perf_event_open`, so it
//! runs where perf events are refused (Railway). Frames in JIT-compiled wasm
//! guest code are named through [`temper_wasm::code_map`].
//!
//! # Settings
//!
//! - `TEMPER_PROFILING_CONTINUOUS` (default off): run the upload loop.
//! - `TEMPER_PROFILING_ENABLED` (default off): serve the on-demand endpoints;
//!   otherwise they answer 503.
//! - `TEMPER_PROFILING_AUTO_UPLOAD` (default off): on-demand captures upload too.
//! - `TEMPER_PROFILING_INTAKE_URL`: where profiles go. Unset, the Datadog
//!   Agent that `DD_TRACE_AGENT_URL`, or `DD_AGENT_HOST` and
//!   `DD_TRACE_AGENT_PORT` (default 8126), name, as Datadog's libraries find
//!   it. With none of these, nothing is uploaded and the continuous loop
//!   captures nothing.
//! - `TEMPER_PROFILING_INTAKE_HEADERS`: `key=value,...` headers sent with each
//!   upload, in the `OTEL_EXPORTER_OTLP_HEADERS` form.
//! - `TEMPER_PROFILING_MAX_SECONDS` (default 120): longest on-demand window.
//! - `TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS` (default 300),
//!   `TEMPER_PROFILING_CONTINUOUS_SECONDS` (default 30) and
//!   `TEMPER_PROFILING_CONTINUOUS_FREQUENCY` (default 100 Hz).
//! - Tags: `DD_SERVICE` (else `OTEL_SERVICE_NAME`), `DD_ENV`, `BUILD_VERSION`
//!   or `DD_VERSION`, and `DD_TAGS`.
//!
//! Heap profiles need a binary whose jemalloc was built with profiling and
//! started with `prof:true` (temper-cli on Linux does both); elsewhere the heap
//! endpoint answers 501 and the loop uploads CPU only.

mod capture;
mod config;
mod continuous;
mod handlers;
mod heap;
mod metrics;
mod symbols;
mod upload;

pub use continuous::spawn_continuous_profiler;
pub use handlers::{
    CpuProfileQuery, cpu_profile_handler, heap_profile_handler, wall_profile_handler,
};
pub use metrics::record_profiling_config;

#[cfg(test)]
mod tests;
