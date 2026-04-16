//! Background sweeper for expired overflow blobs (ADR-0047).
//!
//! Every `interval` the sweeper calls `sweep_expired_blobs` on the platform
//! Turso store, deleting rows whose `expires_at` is in the past. Blobs written
//! without TTL (`expires_at = NULL`) are untouched — only fields that declared
//! `overflow_ttl_seconds` on their `[[state]]` block hit this path.
//!
//! The sweeper is an opt-in platform service. It is spawned from
//! `temper-server::state::ServerState::spawn_blob_sweeper`, which the
//! production startup calls once the Turso store is configured. Simulation
//! configs do not call it, matching ADR-0047's non-goal on simulation
//! visibility (the storage layer is not sim-tracked and `datetime('now')` is
//! a SQL-side wall-clock read).

use std::time::Duration;

use tracing::instrument;

use crate::state::ServerState;

/// Default sweep cadence — 6 hours.
pub const DEFAULT_BLOB_SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Upper bound on rows deleted per sweep call. Large backlogs are cleared
/// over multiple ticks rather than one long-held write lock.
pub const DEFAULT_BLOB_SWEEP_BATCH: u64 = 10_000;

/// Spawn a background task that calls `sweep_expired_blobs` every `interval`.
///
/// Returns the task handle; dropping it aborts the task. In production startup
/// the handle is stored alongside other long-running service tasks.
///
/// Environment knobs (read once at spawn):
/// - `TEMPER_BLOB_SWEEP_DISABLED=1` short-circuits to a no-op future (used by
///   tests / simulation runners that want to suppress the background write).
/// - `TEMPER_BLOB_SWEEP_INTERVAL_SECS` overrides the 6h cadence (min 60s).
/// - `TEMPER_BLOB_SWEEP_BATCH` overrides the 10k per-tick delete cap.
///
/// See ADR-0047.
#[instrument(skip_all, fields(otel.name = "blob_sweeper.spawn"))]
pub fn spawn_blob_sweeper(state: ServerState) -> tokio::task::JoinHandle<()> {
    // determinism-ok: read once at startup to configure a production-only task
    let disabled = std::env::var("TEMPER_BLOB_SWEEP_DISABLED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if disabled {
        tracing::info!("blob sweeper disabled via TEMPER_BLOB_SWEEP_DISABLED");
        return tokio::spawn(async {});
    }

    let interval = std::env::var("TEMPER_BLOB_SWEEP_INTERVAL_SECS") // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s >= 60)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_BLOB_SWEEP_INTERVAL);

    let batch = std::env::var("TEMPER_BLOB_SWEEP_BATCH") // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&b| b > 0)
        .unwrap_or(DEFAULT_BLOB_SWEEP_BATCH);

    tracing::info!(
        interval_secs = interval.as_secs(),
        batch,
        "blob sweeper starting"
    );

    tokio::spawn(async move {
        // determinism-ok: background operational task; simulation configs opt out via env var
        let mut ticker = tokio::time::interval(interval);
        // Skip the initial immediate tick so startup doesn't sweep before
        // verification finishes.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            sweep_once(&state, batch).await;
        }
    })
}

async fn sweep_once(state: &ServerState, batch: u64) {
    // In single-DB Turso mode the platform store IS the shared store for all
    // tenants, so a single sweep covers everything. Tenant-routed deployments
    // will need a second pass per routed tenant DB; that can be layered on
    // when the use case appears.
    let mut iterations = 0u64;
    let total_deleted = if let Some(store) = state.platform_persistent_store() {
        drain(store, batch, &mut iterations).await
    } else {
        0
    };

    if total_deleted > 0 {
        tracing::info!(total_deleted, iterations, "blob sweeper tick completed");
    }
}

async fn drain(
    store: &temper_store_turso::TursoEventStore,
    batch: u64,
    iterations: &mut u64,
) -> u64 {
    let mut total = 0u64;
    loop {
        match store.sweep_expired_blobs(batch).await {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                *iterations += 1;
                if n < batch {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "sweep_expired_blobs failed; retrying next tick");
                break;
            }
        }
    }
    total
}
