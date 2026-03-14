//! Runtime OTEL metrics sampling for server process health.

use std::time::Duration;

use opentelemetry::global;
use opentelemetry::metrics::UpDownCounter;

use super::ServerState;

#[derive(Clone, Copy, Default)]
struct RuntimeMetricSample {
    process_resident_memory_bytes: Option<i64>,
    active_actors: i64,
    active_entities: i64,
}

struct RuntimeMetricInstruments {
    process_resident_memory_bytes: UpDownCounter<i64>,
    active_actors: UpDownCounter<i64>,
    active_entities: UpDownCounter<i64>,
}

impl RuntimeMetricInstruments {
    fn new() -> Self {
        let meter = global::meter("temper-runtime");
        Self {
            process_resident_memory_bytes: meter
                .i64_up_down_counter("process_resident_memory_bytes")
                .with_unit("By")
                .with_description("Resident memory used by the process.")
                .build(),
            active_actors: meter
                .i64_up_down_counter("temper_active_actors")
                .with_description("Number of active in-memory actors.")
                .build(),
            active_entities: meter
                .i64_up_down_counter("temper_active_entities")
                .with_description("Number of active indexed entities.")
                .build(),
        }
    }

    fn record(&self, sample: RuntimeMetricSample, prev: &mut RuntimeMetricSample) {
        if let Some(current_rss) = sample.process_resident_memory_bytes {
            let prev_rss = prev.process_resident_memory_bytes.unwrap_or_default();
            self.process_resident_memory_bytes
                .add(current_rss.saturating_sub(prev_rss), &[]);
            prev.process_resident_memory_bytes = Some(current_rss);
        }

        self.active_actors
            .add(sample.active_actors.saturating_sub(prev.active_actors), &[]);
        prev.active_actors = sample.active_actors;

        self.active_entities.add(
            sample.active_entities.saturating_sub(prev.active_entities),
            &[],
        );
        prev.active_entities = sample.active_entities;
    }
}

impl RuntimeMetricSample {
    fn collect(state: &ServerState) -> Self {
        Self {
            process_resident_memory_bytes: read_process_resident_memory_bytes().map(|v| v as i64),
            active_actors: state.active_actor_count() as i64,
            active_entities: state.active_entity_count() as i64,
        }
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
            let mut previous = RuntimeMetricSample::default();
            let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // consume immediate tick

            loop {
                ticker.tick().await;
                let sample = RuntimeMetricSample::collect(&state);
                instruments.record(sample, &mut previous);
            }
        });
    }
}

fn read_process_resident_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    if let Some(bytes) = read_linux_vm_rss_bytes() {
        return Some(bytes);
    }

    read_ps_rss_bytes()
}

#[cfg(target_os = "linux")]
fn read_linux_vm_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?; // determinism-ok: procfs RSS read
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kb = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(kb.saturating_mul(1024))
}

fn read_ps_rss_bytes() -> Option<u64> {
    let pid = std::process::id().to_string(); // determinism-ok: ps RSS read
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let kb = stdout.trim().parse::<u64>().ok()?;
    Some(kb.saturating_mul(1024))
}
