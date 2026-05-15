//! CPU profile capture for the Temper runtime (ADR-0055).
//!
//! Exposes on-demand CPU profiling via an admin HTTP endpoint and an optional
//! continuous capture loop. An operator can curl
//! `/_admin/profile/cpu?seconds=30` during a burst; production can also set
//! `TEMPER_PROFILING_CONTINUOUS=true` plus auto-upload so profiles stay fresh
//! without a human catching the hot window.
//!
//! This is the primary W8 capability per the Datadog-instrumentation plan.
//! The continuous loop here is deliberately small and opt-in; if Datadog ships
//! a production-ready Rust SDK, this module can hand off capture/upload while
//! keeping the same freshness metrics.
//!
//! # Env gates
//!
//! - `TEMPER_PROFILING_ENABLED` (default `false` until canary finishes):
//!   master switch. When false, the admin endpoint returns 503.
//! - `TEMPER_PROFILING_MAX_SECONDS` (default `120`): hard cap on window
//!   length so a caller cannot tie up a runtime thread for 10 minutes.
//! - `TEMPER_PROFILING_AUTO_UPLOAD` (default `false`): push captured profiles
//!   to the local Datadog Agent profile intake.
//! - `TEMPER_PROFILING_CONTINUOUS` (default `false`): start a bounded periodic
//!   CPU capture loop. Requires auto-upload so samples are not discarded.
//! - `TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS` (default `300`): interval
//!   between periodic captures.
//! - `TEMPER_PROFILING_CONTINUOUS_SECONDS` (default `30`): capture window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use pprof::protos::Message;
use serde::Deserialize;
use tokio::spawn as spawn_observability_task; // determinism-ok: production-only profiler tasks
use tokio::time::sleep as sleep_for_profile_capture; // determinism-ok: bounded profiler capture window

mod metrics;

pub use metrics::record_profiling_config;
use metrics::{
    current_unix_seconds, profile_attrs, profile_stage_attrs, profiling_metrics,
    record_capture_error,
};

/// Resolve `TEMPER_PROFILING_ENABLED` at call time. Startup-read semantics
/// are unnecessary — the toggle is intentionally dynamic so ops can flip
/// it on a live replica without redeploy.
fn profiling_enabled() -> bool {
    std::env::var("TEMPER_PROFILING_ENABLED") // determinism-ok: observability toggle
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn max_window_seconds() -> u64 {
    std::env::var("TEMPER_PROFILING_MAX_SECONDS") // determinism-ok: observability tuning
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120)
        .clamp(5, 600)
}

/// Whether to attempt automatic upload to the Datadog Agent profiling
/// intake on every capture. When off, the operator curls the endpoint
/// manually and uploads separately. When on, the profile is both
/// returned in the HTTP response AND pushed to the Agent.
fn auto_upload_enabled() -> bool {
    std::env::var("TEMPER_PROFILING_AUTO_UPLOAD") // determinism-ok: observability toggle
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn continuous_enabled() -> bool {
    std::env::var("TEMPER_PROFILING_CONTINUOUS") // determinism-ok: observability toggle
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn continuous_interval_seconds() -> u64 {
    std::env::var("TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS") // determinism-ok: observability tuning
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300)
        .clamp(60, 3_600)
}

fn continuous_window_seconds() -> u64 {
    std::env::var("TEMPER_PROFILING_CONTINUOUS_SECONDS") // determinism-ok: observability tuning
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30)
        .clamp(5, max_window_seconds())
}

fn continuous_frequency() -> i32 {
    std::env::var("TEMPER_PROFILING_CONTINUOUS_FREQUENCY") // determinism-ok: observability tuning
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(100)
        .clamp(10, 500)
}

/// Datadog Agent profiling intake URL. Defaults to the local Agent.
fn agent_intake_url() -> String {
    let host = std::env::var("DD_AGENT_HOST") // determinism-ok: production Datadog Agent endpoint
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("DD_TRACE_AGENT_PORT") // determinism-ok: production Datadog Agent endpoint
        .unwrap_or_else(|_| "8126".to_string());
    format!("http://{host}:{port}/profiling/v1/input")
}

async fn capture_profile_body(
    seconds: u64,
    frequency: i32,
    profile_type: &str,
    mode: &str,
) -> Result<Vec<u8>, String> {
    let capture_started = Instant::now(); // determinism-ok: observability duration only
    profiling_metrics()
        .captures_started
        .add(1, &profile_attrs(profile_type, mode));

    let guard = match pprof::ProfilerGuardBuilder::default()
        .frequency(frequency)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
    {
        Ok(g) => g,
        Err(e) => {
            record_capture_error(profile_type, mode, "profiler_start");
            return Err(format!("profiler start failed: {e}"));
        }
    };

    // determinism-ok: observability-only sleep. Profiler samples in the
    // background while the task yields back to the runtime.
    sleep_for_profile_capture(Duration::from_secs(seconds)).await;

    let report = match guard.report().build() {
        Ok(r) => r,
        Err(e) => {
            record_capture_error(profile_type, mode, "report_build");
            return Err(format!("report build failed: {e}"));
        }
    };

    let profile = match report.pprof() {
        Ok(p) => p,
        Err(e) => {
            record_capture_error(profile_type, mode, "pprof_conversion");
            return Err(format!("pprof conversion failed: {e}"));
        }
    };

    let mut body = Vec::new();
    if let Err(e) = profile.write_to_vec(&mut body) {
        record_capture_error(profile_type, mode, "pprof_serialize");
        return Err(format!("pprof serialize failed: {e}"));
    }

    let attrs = profile_attrs(profile_type, mode);
    let elapsed_ms = capture_started.elapsed().as_secs_f64() * 1_000.0;
    let metrics = profiling_metrics();
    metrics.captures_completed.add(1, &attrs);
    metrics.capture_duration_ms.record(elapsed_ms, &attrs);
    metrics
        .last_capture_unix_seconds
        .record(current_unix_seconds(), &attrs);

    Ok(body)
}

async fn upload_to_agent(pprof_bytes: Vec<u8>, profile_type: &str, mode: &str) {
    let url = agent_intake_url();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "profile uploader: failed to build HTTP client");
            profiling_metrics()
                .upload_errors
                .add(1, &profile_stage_attrs(profile_type, mode, "client_build"));
            return;
        }
    };

    // The Datadog profiling intake expects a multipart form with:
    //   - `tags[]`: service, env, version, host, runtime-id, etc.
    //   - `data[profile.pprof]`: the gzipped pprof blob.
    //
    // The pprof crate's `.pprof()` method already returns a pprof.proto
    // body; Datadog accepts raw pprof.
    let service = std::env::var("DD_SERVICE") // determinism-ok: production profile metadata tag
        .unwrap_or_else(|_| "temper".to_string());
    let env = std::env::var("DD_ENV") // determinism-ok: production profile metadata tag
        .unwrap_or_else(|_| "prod".to_string());
    let version = std::env::var("BUILD_VERSION") // determinism-ok: production profile metadata tag
        .or_else(|_| std::env::var("DD_VERSION")) // determinism-ok: production profile metadata tag
        .unwrap_or_else(|_| "dev".to_string());

    let form = reqwest::multipart::Form::new()
        .text("tags[]", format!("service:{service}"))
        .text("tags[]", format!("env:{env}"))
        .text("tags[]", format!("version:{version}"))
        .text("tags[]", format!("profile.component:{profile_type}"))
        .text("tags[]", format!("profile.mode:{mode}"))
        .part(
            "data[profile.pprof]",
            reqwest::multipart::Part::bytes(pprof_bytes)
                .file_name("profile.pprof")
                .mime_str("application/octet-stream")
                .unwrap_or_else(|_| reqwest::multipart::Part::text("")),
        );

    match client.post(&url).multipart(form).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(url = %url, "profile uploaded to Datadog Agent intake");
            profiling_metrics()
                .profiles_uploaded
                .add(1, &profile_attrs(profile_type, mode));
        }
        Ok(resp) => {
            tracing::warn!(
                url = %url,
                status = %resp.status(),
                "profile upload non-2xx",
            );
            profiling_metrics().upload_errors.add(
                1,
                &profile_stage_attrs(profile_type, mode, "non_success_status"),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "profile upload failed");
            profiling_metrics()
                .upload_errors
                .add(1, &profile_stage_attrs(profile_type, mode, "request_error"));
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CpuProfileQuery {
    /// Sampling window length in seconds. Clamped to [1, TEMPER_PROFILING_MAX_SECONDS].
    #[serde(default = "default_seconds")]
    pub seconds: u64,
    /// Sampling frequency in Hz (default 100).
    #[serde(default = "default_frequency")]
    pub frequency: i32,
}

fn default_seconds() -> u64 {
    30
}

fn default_frequency() -> i32 {
    100
}

/// `GET /admin/profile/cpu?seconds=30&frequency=100`
///
/// Returns a pprof-format protobuf body (Content-Type:
/// `application/vnd.google.protobuf`). Suitable for upload to Datadog's
/// profile intake or for local analysis via `pprof`.
pub async fn cpu_profile_handler(Query(q): Query<CpuProfileQuery>) -> Response {
    if !profiling_enabled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "profiling disabled — set TEMPER_PROFILING_ENABLED=true to enable",
        )
            .into_response();
    }

    let max_s = max_window_seconds();
    let seconds = q.seconds.clamp(1, max_s);
    let frequency = q.frequency.clamp(10, 500);

    tracing::info!(seconds, frequency, "ADR-0055: starting CPU profile capture");

    let body = match capture_profile_body(seconds, frequency, "cpu", "on_demand").await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(error = %e, "CPU profile capture failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                e,
            )
                .into_response();
        }
    };

    tracing::info!(
        bytes = body.len(),
        seconds,
        frequency,
        "ADR-0055: CPU profile capture complete"
    );

    if auto_upload_enabled() {
        let body_for_upload = body.clone();
        spawn_observability_task(async move {
            // determinism-ok: observability-only upload task
            upload_to_agent(body_for_upload, "cpu", "on_demand").await;
        });
    }

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.google.protobuf".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"cpu-profile-{seconds}s.pb\""),
            ),
        ],
        body,
    )
        .into_response()
}

/// Start opt-in continuous CPU profiling.
///
/// The loop is intentionally disabled unless profiling, continuous mode, and
/// auto-upload are all enabled. This avoids quietly burning CPU or collecting
/// profiles that never reach Datadog.
pub fn spawn_continuous_profiler() {
    record_profiling_config();

    if !profiling_enabled() || !continuous_enabled() {
        return;
    }

    if !auto_upload_enabled() {
        tracing::warn!(
            "TEMPER_PROFILING_CONTINUOUS=true but TEMPER_PROFILING_AUTO_UPLOAD is not enabled; continuous profiling not started"
        );
        return;
    }

    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        tracing::debug!("continuous profiling loop already started");
        return;
    }

    let interval_secs = continuous_interval_seconds();
    let seconds = continuous_window_seconds();
    let frequency = continuous_frequency();
    tracing::info!(
        interval_secs,
        seconds,
        frequency,
        "starting continuous CPU profiler"
    );

    spawn_observability_task(async move {
        // determinism-ok: observability-only periodic profiler loop
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            record_profiling_config();

            if !profiling_enabled() || !auto_upload_enabled() {
                tracing::warn!(
                    "continuous profiler skipped because profiling or auto-upload is disabled"
                );
                continue;
            }

            match capture_profile_body(seconds, frequency, "cpu", "continuous").await {
                Ok(body) => {
                    upload_to_agent(body, "cpu", "continuous").await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "continuous CPU profile capture failed");
                }
            }
        }
    });
}

/// `GET /admin/profile/wall?seconds=30&frequency=100`
///
/// Wall-clock profile (samples on every tick, not only on-CPU). Captures
/// time spent in syscalls, I/O wait, lock contention — the primary signal
/// for temper#146 investigation.
///
/// Under the hood: pprof-rs doesn't have a distinct "wall-clock" mode
/// (CPU sampling is its only mode), but we expose a separate route so the
/// operator distinguishes the intent in Datadog and the `profile.component`
/// upload tag differentiates them. When pprof-rs adds wall-clock support,
/// the implementation switches without any API change to the endpoint.
pub async fn wall_profile_handler(Query(q): Query<CpuProfileQuery>) -> Response {
    if !profiling_enabled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "profiling disabled — set TEMPER_PROFILING_ENABLED=true to enable",
        )
            .into_response();
    }

    let max_s = max_window_seconds();
    let seconds = q.seconds.clamp(1, max_s);
    let frequency = q.frequency.clamp(10, 500);

    tracing::info!(
        seconds,
        frequency,
        "ADR-0055: starting wall-clock profile capture"
    );

    let body = match capture_profile_body(seconds, frequency, "wall", "on_demand").await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(error = %e, "wall-clock profile capture failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                e,
            )
                .into_response();
        }
    };

    if auto_upload_enabled() {
        let body_for_upload = body.clone();
        spawn_observability_task(async move {
            // determinism-ok: observability-only upload task
            upload_to_agent(body_for_upload, "wall", "on_demand").await;
        });
    }

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.google.protobuf".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"wall-profile-{seconds}s.pb\""),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests;
