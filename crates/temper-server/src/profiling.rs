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

use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Counter;
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

/// Whether to attempt automatic upload to the Datadog Agent profiling
/// intake on every capture. When off, the operator curls the endpoint
/// manually and uploads separately. When on, the profile is both
/// returned in the HTTP response AND pushed to the Agent.
fn auto_upload_enabled() -> bool {
    std::env::var("TEMPER_PROFILING_AUTO_UPLOAD")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Datadog Agent profiling intake URL. Defaults to the local Agent.
fn agent_intake_url() -> String {
    let host = std::env::var("DD_AGENT_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("DD_TRACE_AGENT_PORT").unwrap_or_else(|_| "8126".to_string());
    format!("http://{host}:{port}/profiling/v1/input")
}

struct ProfilingMetrics {
    profiles_uploaded: Counter<u64>,
    upload_errors: Counter<u64>,
}

fn profiling_metrics() -> &'static ProfilingMetrics {
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
        }
    })
}

async fn upload_to_agent(pprof_bytes: Vec<u8>, profile_type: &str) {
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
                .add(1, &[KeyValue::new("stage", "client_build")]);
            return;
        }
    };

    // The Datadog profiling intake expects a multipart form with:
    //   - `tags[]`: service, env, version, host, runtime-id, etc.
    //   - `data[profile.pprof]`: the gzipped pprof blob.
    //
    // The pprof crate's `.pprof()` method already returns a pprof.proto
    // body; Datadog accepts raw pprof.
    let service = std::env::var("DD_SERVICE").unwrap_or_else(|_| "temper".to_string());
    let env = std::env::var("DD_ENV").unwrap_or_else(|_| "prod".to_string());
    let version = std::env::var("BUILD_VERSION")
        .or_else(|_| std::env::var("DD_VERSION"))
        .unwrap_or_else(|_| "dev".to_string());

    let form = reqwest::multipart::Form::new()
        .text("tags[]", format!("service:{service}"))
        .text("tags[]", format!("env:{env}"))
        .text("tags[]", format!("version:{version}"))
        .text("tags[]", format!("profile.component:{profile_type}"))
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
            profiling_metrics().profiles_uploaded.add(
                1,
                &[KeyValue::new("profile_type", profile_type.to_string())],
            );
        }
        Ok(resp) => {
            tracing::warn!(
                url = %url,
                status = %resp.status(),
                "profile upload non-2xx",
            );
            profiling_metrics()
                .upload_errors
                .add(1, &[KeyValue::new("stage", "non_success_status")]);
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "profile upload failed");
            profiling_metrics()
                .upload_errors
                .add(1, &[KeyValue::new("stage", "request_error")]);
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

    if auto_upload_enabled() {
        let body_for_upload = body.clone();
        tokio::spawn(async move {
            upload_to_agent(body_for_upload, "cpu").await;
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

    let guard = match pprof::ProfilerGuardBuilder::default()
        .frequency(frequency)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
    {
        Ok(g) => g,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                format!("profiler start failed: {e}"),
            )
                .into_response();
        }
    };

    tokio::time::sleep(Duration::from_secs(seconds)).await;

    let profile = match guard.report().build().and_then(|r| r.pprof()) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                format!("pprof build failed: {e}"),
            )
                .into_response();
        }
    };
    let mut body = Vec::new();
    if let Err(e) = profile.write_to_vec(&mut body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain")],
            format!("pprof serialize failed: {e}"),
        )
            .into_response();
    }

    if auto_upload_enabled() {
        let body_for_upload = body.clone();
        tokio::spawn(async move {
            upload_to_agent(body_for_upload, "wall").await;
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
