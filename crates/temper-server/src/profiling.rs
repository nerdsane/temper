//! CPU profile capture for the Temper runtime (ADR-0055).
//!
//! Exposes on-demand CPU profiling via an admin HTTP endpoint. An operator
//! curls `/admin/profile/cpu?seconds=30` during a burst; the server runs a
//! pprof-rs profiler for the requested window and returns a pprof-format
//! protobuf blob. The blob can be uploaded to Datadog's profile intake
//! endpoint or inspected locally with `go tool pprof` / `pprof -http`.
//!
//! This is the primary W8 capability per the Datadog-instrumentation plan.
//! Continuous always-on profiling will arrive when Datadog ships a
//! production-ready Rust SDK; until then, on-demand capture is enough for
//! temper#146-style investigations.
//!
//! # Env gates
//!
//! - `TEMPER_PROFILING_ENABLED` (default `false` until canary finishes):
//!   master switch. When false, the admin endpoint returns 503.
//! - `TEMPER_PROFILING_MAX_SECONDS` (default `120`): hard cap on window
//!   length so a caller cannot tie up a runtime thread for 10 minutes.

use std::time::Duration;

use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use pprof::protos::Message;
use serde::Deserialize;

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

    tracing::info!(
        seconds,
        frequency,
        "ADR-0055: starting CPU profile capture"
    );

    let guard = match pprof::ProfilerGuardBuilder::default()
        .frequency(frequency)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
    {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "failed to start profiler");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                format!("profiler start failed: {e}"),
            )
                .into_response();
        }
    };

    // determinism-ok: observability-only sleep. Profiler samples in the
    // background while the task yields back to the runtime.
    tokio::time::sleep(Duration::from_secs(seconds)).await;

    let report = match guard.report().build() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "profiler report build failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                format!("report build failed: {e}"),
            )
                .into_response();
        }
    };

    let profile = match report.pprof() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "pprof conversion failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                format!("pprof conversion failed: {e}"),
            )
                .into_response();
        }
    };

    let mut body = Vec::new();
    if let Err(e) = profile.write_to_vec(&mut body) {
        tracing::warn!(error = %e, "pprof serialization failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain")],
            format!("pprof serialize failed: {e}"),
        )
            .into_response();
    }

    tracing::info!(
        bytes = body.len(),
        seconds,
        frequency,
        "ADR-0055: CPU profile capture complete"
    );

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.google.protobuf".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"cpu-profile-{seconds}s.pb\""
                ),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_applied() {
        let q = CpuProfileQuery {
            seconds: default_seconds(),
            frequency: default_frequency(),
        };
        assert_eq!(q.seconds, 30);
        assert_eq!(q.frequency, 100);
    }

    use std::sync::Mutex;

    // Cargo runs tests in parallel; env-var mutation races otherwise.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn profiling_toggle_states() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("TEMPER_PROFILING_ENABLED");
        }
        assert!(!profiling_enabled(), "default should be off");

        for truthy in ["1", "true", "yes", "on"] {
            unsafe {
                std::env::set_var("TEMPER_PROFILING_ENABLED", truthy);
            }
            assert!(profiling_enabled(), "{truthy} should enable");
        }

        for falsy in ["0", "false", "no", "off", ""] {
            unsafe {
                std::env::set_var("TEMPER_PROFILING_ENABLED", falsy);
            }
            assert!(!profiling_enabled(), "{falsy:?} should disable");
        }

        unsafe {
            std::env::remove_var("TEMPER_PROFILING_ENABLED");
        }
    }

    #[test]
    fn max_window_clamps() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TEMPER_PROFILING_MAX_SECONDS", "99999");
        }
        assert_eq!(max_window_seconds(), 600);
        unsafe {
            std::env::set_var("TEMPER_PROFILING_MAX_SECONDS", "2");
        }
        assert_eq!(max_window_seconds(), 5);
        unsafe {
            std::env::remove_var("TEMPER_PROFILING_MAX_SECONDS");
        }
    }
}
