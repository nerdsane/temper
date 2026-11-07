//! On-demand profile endpoints, served under `/_admin/profile/*` once the
//! admin router has checked the caller.

use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use tokio::spawn as spawn_observability_task; // determinism-ok: production-only profiler upload

use super::capture::capture_cpu;
use super::config;
use super::heap::{self, HeapError};
use super::upload::{Captured, upload};

/// Query for a sampled CPU capture.
#[derive(Debug, Deserialize)]
pub struct CpuProfileQuery {
    /// Window length in seconds, clamped to [1, `TEMPER_PROFILING_MAX_SECONDS`].
    #[serde(default = "default_seconds")]
    pub seconds: u64,
    /// Sampling frequency in Hz, clamped to [10, 500].
    #[serde(default = "default_frequency")]
    pub frequency: i32,
}

fn default_seconds() -> u64 {
    30
}

fn default_frequency() -> i32 {
    100
}

/// `GET /_admin/profile/cpu?seconds=30&frequency=100`: a pprof CPU profile.
pub async fn cpu_profile_handler(Query(q): Query<CpuProfileQuery>) -> Response {
    sampled_profile(q, "cpu").await
}

/// `GET /_admin/profile/wall?seconds=30&frequency=100`.
///
/// The same CPU-timer capture as `/cpu`, tagged `profile.component:wall` so a
/// capture taken to study waiting can be told apart in Datadog. The timer
/// counts on-CPU time only.
pub async fn wall_profile_handler(Query(q): Query<CpuProfileQuery>) -> Response {
    sampled_profile(q, "wall").await
}

/// `GET /_admin/profile/heap`: a pprof snapshot of the sampled live heap.
/// 501 where the allocator has no heap profile to give.
pub async fn heap_profile_handler() -> Response {
    if let Some(refusal) = disabled() {
        return refusal;
    }
    let started_at = Utc::now(); // determinism-ok: observability profile metadata
    match heap::capture_heap().await {
        Ok(body) => answer(body, "heap", "heap-profile.pb", started_at),
        Err(HeapError::Unavailable(why)) => text(StatusCode::NOT_IMPLEMENTED, why.to_string()),
        Err(HeapError::Failed(why)) => {
            tracing::warn!(error = %why, "heap profile capture failed");
            text(StatusCode::INTERNAL_SERVER_ERROR, why)
        }
    }
}

async fn sampled_profile(q: CpuProfileQuery, profile_type: &'static str) -> Response {
    if let Some(refusal) = disabled() {
        return refusal;
    }
    let seconds = q.seconds.clamp(1, config::max_window_seconds());
    let frequency = q.frequency.clamp(10, 500);
    tracing::info!(seconds, frequency, profile_type, "profile capture starting");
    let started_at = Utc::now(); // determinism-ok: observability profile metadata
    match capture_cpu(seconds, frequency, profile_type, "on_demand").await {
        Ok(body) => answer(
            body,
            profile_type,
            &format!("{profile_type}-profile-{seconds}s.pb"),
            started_at,
        ),
        Err(why) => {
            tracing::warn!(error = %why, profile_type, "profile capture failed");
            text(StatusCode::INTERNAL_SERVER_ERROR, why)
        }
    }
}

fn disabled() -> Option<Response> {
    (!config::on_demand_enabled()).then(|| {
        text(
            StatusCode::SERVICE_UNAVAILABLE,
            "profiling disabled; set TEMPER_PROFILING_ENABLED=true to enable".to_string(),
        )
    })
}

/// Answer with the profile, uploading a copy first when auto-upload is on.
fn answer(
    body: Vec<u8>,
    profile_type: &'static str,
    filename: &str,
    started_at: chrono::DateTime<Utc>,
) -> Response {
    tracing::info!(bytes = body.len(), profile_type, "profile capture complete");
    if config::auto_upload_enabled() {
        let captured = Captured {
            pprof: body.clone(),
            profile_type,
            mode: "on_demand",
            started_at,
            ended_at: Utc::now(), // determinism-ok: observability profile metadata
        };
        spawn_observability_task(upload(captured, config::intake()));
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
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response()
}

fn text(status: StatusCode, message: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain")], message).into_response()
}
