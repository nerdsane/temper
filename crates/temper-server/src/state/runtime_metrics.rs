//! Runtime OTEL metrics sampling for server process health.

use std::time::Duration;

use opentelemetry::global;
use opentelemetry::metrics::Gauge;

use super::ServerState;

struct RuntimeMetricInstruments {
    process_resident_memory_bytes: Gauge<u64>,
    active_actors: Gauge<u64>,
    active_entities: Gauge<u64>,
}

impl RuntimeMetricInstruments {
    fn new() -> Self {
        let meter = global::meter("temper-runtime");
        Self {
            process_resident_memory_bytes: meter
                .u64_gauge("process_resident_memory_bytes")
                .with_unit("By")
                .with_description("Resident memory used by the process.")
                .build(),
            active_actors: meter
                .u64_gauge("temper_active_actors")
                .with_description("Number of active in-memory actors.")
                .build(),
            active_entities: meter
                .u64_gauge("temper_active_entities")
                .with_description("Number of active indexed entities.")
                .build(),
        }
    }

    fn record(&self, state: &ServerState) {
        if let Some(rss) = read_process_resident_memory_bytes() {
            self.process_resident_memory_bytes.record(rss, &[]);
        }
        self.active_actors.record(state.active_actor_count(), &[]);
        self.active_entities
            .record(state.active_entity_count(), &[]);
    }
}

impl ServerState {
    /// Start periodic runtime metric export for process + actor-system state.
    pub fn spawn_runtime_metrics_loop(&self) {
        let interval_secs = std::env::var("TEMPER_RUNTIME_METRICS_INTERVAL_SECS") // determinism-ok: read once at startup
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10)
            .clamp(1, 86_400);

        let state = self.clone();
        tokio::spawn(async move {
            // determinism-ok: background metrics export loop
            let instruments = RuntimeMetricInstruments::new();
            let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // consume immediate tick

            loop {
                ticker.tick().await;
                instruments.record(&state);
            }
        });
    }
}

fn read_process_resident_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    if let Some(bytes) = read_linux_vm_rss_bytes() {
        return Some(bytes);
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }

    #[cfg(target_os = "linux")]
    None
}

#[cfg(target_os = "linux")]
fn read_linux_vm_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?; // determinism-ok: procfs RSS read
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kb = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(kb.saturating_mul(1024))
}
