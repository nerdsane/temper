//! Runtime metrics exported via OpenTelemetry.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::{Gauge, Histogram};
use opentelemetry::{KeyValue, global};
use tokio::time::MissedTickBehavior;

use crate::state::ServerState;

struct RuntimeMetrics {
    process_resident_memory_bytes: Gauge<u64>,
    active_actors: Gauge<u64>,
    active_entities: Gauge<u64>,
    event_replay_duration: Histogram<f64>,
}

fn metrics() -> &'static RuntimeMetrics {
    static METRICS: OnceLock<RuntimeMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        RuntimeMetrics {
            process_resident_memory_bytes: meter
                .u64_gauge("process_resident_memory_bytes")
                .with_description("Resident set size (RSS) memory usage in bytes.")
                .build(),
            active_actors: meter
                .u64_gauge("temper_active_actors")
                .with_description("Number of currently active spawned entity actors.")
                .build(),
            active_entities: meter
                .u64_gauge("temper_active_entities")
                .with_description("Number of hydrated entities currently indexed by tenant.")
                .build(),
            event_replay_duration: meter
                .f64_histogram("temper_event_replay_duration")
                .with_description("Time spent replaying event journals.")
                .build(),
        }
    })
}

/// Register runtime metric instruments.
pub fn init_runtime_metrics() {
    let _ = metrics();
}

/// Spawn a periodic sampler that records process RSS and live actor/entity counts.
pub fn spawn_runtime_metrics_sampler(state: ServerState) {
    init_runtime_metrics();
    tokio::spawn(async move {
        // determinism-ok: runtime observability side-effect for production diagnostics
        let interval_secs = std::env::var("TEMPER_RUNTIME_METRICS_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10)
            .clamp(1, 86_400);
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            record_server_state_metrics(&state);
            if let Some(rss_bytes) = read_process_resident_memory_bytes() {
                record_process_resident_memory_bytes(rss_bytes);
            }
        }
    });
}

/// Record actor and entity counts from the current server state snapshot.
pub fn record_server_state_metrics(state: &ServerState) {
    if let Ok(registry) = state.actor_registry.read() {
        record_active_actor_count(registry.len());
    }
    if let Ok(index) = state.entity_index.read() {
        record_active_entity_counts(&index);
    }
}

/// Record current active actor count.
pub fn record_active_actor_count(count: usize) {
    metrics().active_actors.record(count as u64, &[]);
}

/// Record active entity counts by tenant and global total.
pub fn record_active_entity_counts(index: &BTreeMap<String, BTreeSet<String>>) {
    let mut by_tenant: BTreeMap<String, u64> = BTreeMap::new();
    for (index_key, ids) in index {
        if let Some((tenant, _entity_type)) = index_key.split_once(':') {
            *by_tenant.entry(tenant.to_string()).or_insert(0) += ids.len() as u64;
        }
    }

    let total: u64 = by_tenant.values().copied().sum();
    metrics().active_entities.record(total, &[]);

    for (tenant, count) in by_tenant {
        metrics()
            .active_entities
            .record(count, &[KeyValue::new("tenant", tenant)]);
    }
}

/// Record event replay duration.
pub fn record_event_replay_duration(duration: Duration, tenant: &str, entity_type: &str) {
    metrics().event_replay_duration.record(
        duration.as_secs_f64(),
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
        ],
    );
}

/// Record process resident memory usage.
pub fn record_process_resident_memory_bytes(bytes: u64) {
    metrics().process_resident_memory_bytes.record(bytes, &[]);
}

/// Read process resident memory (RSS) in bytes from Linux procfs.
#[cfg(target_os = "linux")]
pub fn read_process_resident_memory_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let vm_rss_line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let mut parts = vm_rss_line.split_whitespace();
    let _label = parts.next()?;
    let value_kb = parts.next()?.parse::<u64>().ok()?;
    Some(value_kb.saturating_mul(1024))
}

/// Read process resident memory (RSS) in bytes from Linux procfs.
#[cfg(not(target_os = "linux"))]
pub fn read_process_resident_memory_bytes() -> Option<u64> {
    None
}
