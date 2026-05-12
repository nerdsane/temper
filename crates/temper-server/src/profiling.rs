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
use chrono::{DateTime, SecondsFormat, Utc};
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Counter;
use pprof::protos::Message;
use serde::Deserialize;
use serde_json::json;

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

fn non_empty_env(var_name: &str) -> Option<String> {
    std::env::var(var_name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn service_name() -> String {
    non_empty_env("DD_SERVICE").unwrap_or_else(|| "temper".to_string())
}

fn deployment_environment() -> String {
    non_empty_env("DD_ENV").unwrap_or_else(|| "prod".to_string())
}

fn service_version() -> String {
    non_empty_env("BUILD_VERSION")
        .or_else(|| non_empty_env("DD_VERSION"))
        .unwrap_or_else(|| "dev".to_string())
}

fn profile_filename(profile_type: &str) -> String {
    format!("{profile_type}.pprof")
}

fn profile_tags(profile_type: &str) -> Vec<String> {
    vec![
        format!("service:{}", service_name()),
        format!("env:{}", deployment_environment()),
        format!("version:{}", service_version()),
        format!("runtime-id:{}", temper_observe::otel::runtime_id()),
        "runtime:rust".to_string(),
        format!("profile.component:{profile_type}"),
    ]
}

fn profile_upload_event_json(
    profile_type: &str,
    filename: &str,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) -> serde_json::Value {
    let activation = if non_empty_env("DD_PROFILING_ENABLED").as_deref() == Some("auto") {
        "auto"
    } else {
        "manual"
    };

    json!({
        "version": "4",
        "family": "rust",
        "start": started_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        "end": ended_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        "attachments": [filename],
        "tags_profiler": profile_tags(profile_type).join(","),
        "info": {
            "profiler": {
                "activation": activation,
                "ssi": {
                    "mechanism": "none"
                },
                "settings": {
                    "profile_type": profile_type,
                    "profile_source": "pprof-rs"
                }
            }
        }
    })
}

async fn upload_to_agent(
    pprof_bytes: Vec<u8>,
    profile_type: &str,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) {
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

    // The Datadog profiling intake expects the same multipart envelope used by
    // first-party profilers: an `event.json` part plus profile attachments
    // named exactly as listed in the event's `attachments` array.
    let filename = profile_filename(profile_type);
    let event = profile_upload_event_json(profile_type, &filename, started_at, ended_at);
    let event_json = match serde_json::to_vec(&event) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(error = %e, "profile uploader: failed to encode event.json");
            profiling_metrics()
                .upload_errors
                .add(1, &[KeyValue::new("stage", "event_encode")]);
            return;
        }
    };
    let form = reqwest::multipart::Form::new()
        .part(
            filename.clone(),
            reqwest::multipart::Part::bytes(pprof_bytes)
                .file_name(filename.clone())
                .mime_str("application/octet-stream")
                .expect("static profile upload MIME type is valid"),
        )
        .part(
            "event",
            reqwest::multipart::Part::bytes(event_json)
                .file_name("event.json")
                .mime_str("application/json")
                .expect("static profile event MIME type is valid"),
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
    let started_at = Utc::now();

    // determinism-ok: observability-only sleep. Profiler samples in the
    // background while the task yields back to the runtime.
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let ended_at = Utc::now();

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
            upload_to_agent(body_for_upload, "cpu", started_at, ended_at).await;
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
    let started_at = Utc::now();

    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let ended_at = Utc::now();

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
            upload_to_agent(body_for_upload, "wall", started_at, ended_at).await;
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
