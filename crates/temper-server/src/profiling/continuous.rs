//! The continuous loop: every interval, a CPU window and a heap snapshot,
//! each uploaded to the intake.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::spawn as spawn_observability_task; // determinism-ok: production-only profiler loop

use super::capture::capture_cpu;
use super::config::Intake;
use super::metrics::record_profiling_config;
use super::upload::{Captured, upload};
use super::{config, heap};

/// Start the loop when `TEMPER_PROFILING_CONTINUOUS` is on. Each cycle runs
/// only while an intake is configured. Called once at serve start; a second
/// call is a no-op.
pub fn spawn_continuous_profiler() {
    record_profiling_config();
    if !config::continuous_enabled() {
        return;
    }
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }

    let interval = config::continuous_interval_seconds();
    let heap_available = heap::available();
    tracing::info!(
        interval_secs = interval,
        window_secs = config::continuous_window_seconds(),
        frequency = config::continuous_frequency(),
        heap = heap_available,
        intake = config::intake().map_or_else(|| "none".to_string(), |intake| intake.url),
        "continuous profiler starting"
    );
    if config::intake().is_none() {
        tracing::warn!(
            "continuous profiling is on but no intake is set (TEMPER_PROFILING_INTAKE_URL, DD_TRACE_AGENT_URL or DD_AGENT_HOST): nothing is captured until one is"
        );
    }

    spawn_observability_task(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            record_profiling_config();
            if !config::continuous_enabled() {
                continue;
            }
            let Some(intake) = config::intake() else {
                continue;
            };
            let cycle_started = Utc::now(); // determinism-ok: observability profile metadata
            capture_and_upload_cpu(intake.clone()).await;
            if heap_available {
                capture_and_upload_heap(cycle_started, intake).await;
            }
        }
    });
}

async fn capture_and_upload_cpu(intake: Intake) {
    let seconds = config::continuous_window_seconds();
    let started_at = Utc::now(); // determinism-ok: observability profile metadata
    match capture_cpu(seconds, config::continuous_frequency(), "cpu", "continuous").await {
        Ok(pprof) => {
            upload(
                Captured {
                    pprof,
                    profile_type: "cpu",
                    mode: "continuous",
                    started_at,
                    ended_at: Utc::now(), // determinism-ok: observability profile metadata
                },
                intake,
            )
            .await
        }
        Err(e) => tracing::warn!(error = %e, "continuous CPU profile failed"),
    }
}

/// The heap as it stands at the end of the cycle that began at
/// `cycle_started`; the cycle is the window the upload reports.
async fn capture_and_upload_heap(cycle_started: DateTime<Utc>, intake: Intake) {
    match heap::capture_heap().await {
        Ok(pprof) => {
            upload(
                Captured {
                    pprof,
                    profile_type: "heap",
                    mode: "continuous",
                    started_at: cycle_started,
                    ended_at: Utc::now(), // determinism-ok: observability profile metadata
                },
                intake,
            )
            .await
        }
        Err(e) => tracing::warn!(error = ?e, "continuous heap profile failed"),
    }
}
