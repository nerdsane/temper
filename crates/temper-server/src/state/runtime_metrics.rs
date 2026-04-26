//! Runtime OTEL metrics sampling for server process health.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Gauge;

use super::ServerState;

struct RuntimeMetricInstruments {
    /// Canary: always 1; confirms the metric export pipeline is alive.
    up: Gauge<u64>,
    process_resident_memory_bytes: Gauge<u64>,
    active_actors: Gauge<u64>,
    indexed_entities: Gauge<u64>,
    projected_entities: Gauge<u64>,
    projection_coverage_ratio: Gauge<f64>,
    durable_store_timeout: Duration,
}

impl RuntimeMetricInstruments {
    fn new() -> Self {
        let meter = global::meter("temper-runtime");
        Self {
            up: meter
                .u64_gauge("temper_up")
                .with_description(
                    "Always 1 — canary confirming the metric export pipeline is alive.",
                )
                .build(),
            process_resident_memory_bytes: meter
                .u64_gauge("process_resident_memory_bytes")
                .with_unit("By")
                .with_description("Resident memory used by the process.")
                .build(),
            active_actors: meter
                .u64_gauge("temper_active_actors")
                .with_description("Number of active in-memory actors.")
                .build(),
            indexed_entities: meter
                .u64_gauge("temper_indexed_entities")
                .with_description("Number of entities currently present in the in-memory query-plane index.")
                .build(),
            projected_entities: meter
                .u64_gauge("temper_projected_entities")
                .with_description("Number of entities present in the durable query-plane catalog.")
                .build(),
            projection_coverage_ratio: meter
                .f64_gauge("temper_projection_coverage_ratio")
                .with_description(
                    "Projected entity count divided by indexed entity count for the current process.",
                )
                .build(),
            durable_store_timeout: configured_duration(
                "TEMPER_RUNTIME_METRICS_STORE_TIMEOUT_MS",
                2_000,
                100,
                60_000,
            ),
        }
    }

    async fn record(&self, state: &ServerState) {
        self.up.record(1, &[]);
        if let Some(rss) = read_process_resident_memory_bytes() {
            self.process_resident_memory_bytes.record(rss, &[]);
        }
        self.active_actors.record(state.active_actor_count(), &[]);
        let indexed_by_tenant = state.active_entity_counts_by_tenant();
        let indexed_total: u64 = indexed_by_tenant.values().copied().sum();
        self.indexed_entities.record(indexed_total, &[]);

        if let Some(store) = state.event_store.as_ref()
            && let Ok(Ok(Some(projected_by_tenant))) = tokio::time::timeout(
                self.durable_store_timeout,
                store.projected_entity_counts_by_tenant(),
            )
            .await
        {
            let projected_total: u64 = projected_by_tenant.iter().map(|(_, count)| *count).sum();
            self.projected_entities.record(projected_total, &[]);

            let coverage_total = coverage_ratio(projected_total, indexed_total);
            self.projection_coverage_ratio.record(coverage_total, &[]);

            for (tenant, count) in projected_by_tenant {
                self.projected_entities
                    .record(count, &[KeyValue::new("tenant", tenant.clone())]);
                let indexed = indexed_by_tenant.get(&tenant).copied().unwrap_or(0);
                self.projection_coverage_ratio.record(
                    coverage_ratio(count, indexed),
                    &[KeyValue::new("tenant", tenant)],
                );
            }
        }

        // ADR-0051: admission gauges — sampled alongside other runtime gauges
        // rather than emitted on the dispatch hot path.
        for (tenant, entity_type, action, active, queue_depth) in
            state.admission.snapshot_for_metrics().await
        {
            crate::runtime_metrics::record_admission_active_permits(
                &tenant,
                &entity_type,
                &action,
                active,
            );
            crate::runtime_metrics::record_admission_queue_depth(
                &tenant,
                &entity_type,
                &action,
                queue_depth,
            );
        }

        // ADR-0049: pending timer gauge — emitted per entity type.
        for (entity_type, count) in state.state_timeout_tracker.pending_snapshot() {
            crate::runtime_metrics::record_scheduler_pending_timers(&entity_type, count);
        }

        // ADR-0050: allow_indefinite_states governance gauge — emitted per
        // entity type from the currently loaded spec registry.
        for (entity_type, count) in allow_indefinite_state_counts(state) {
            crate::runtime_metrics::record_spec_allow_indefinite_states(&entity_type, count);
        }
    }
}

/// Walk every registered entity spec and count `allow_indefinite_states`
/// per entity type. Result is cross-tenant (the last tenant's value wins on
/// collision — acceptable because specs with the same name across tenants
/// are semantically the same spec).
fn allow_indefinite_state_counts(state: &ServerState) -> Vec<(String, u64)> {
    let Ok(registry) = state.registry.read() else {
        return Vec::new();
    };
    let mut out: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for tenant in registry.tenant_ids() {
        for entity_type in registry.entity_types(tenant) {
            if let Some(spec) = registry.get_spec(tenant, entity_type) {
                let count = spec.automaton.automaton.allow_indefinite_states.len() as u64;
                out.insert(entity_type.to_string(), count);
            }
        }
    }
    out.into_iter().collect()
}

impl ServerState {
    /// Start periodic runtime metric export for process + actor-system state.
    pub fn spawn_runtime_metrics_loop(&self) {
        let interval_secs = configured_u64("TEMPER_RUNTIME_METRICS_INTERVAL_SECS", 60, 1, 86_400);

        let state = self.clone();
        tokio::spawn(async move {
            // determinism-ok: background metrics export loop
            let instruments = RuntimeMetricInstruments::new();
            let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // consume immediate tick

            loop {
                ticker.tick().await;
                instruments.record(&state).await;
            }
        });
    }
}

fn configured_duration(env_name: &str, default_ms: u64, min_ms: u64, max_ms: u64) -> Duration {
    Duration::from_millis(configured_u64(env_name, default_ms, min_ms, max_ms))
}

fn configured_u64(env_name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(env_name) // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn coverage_ratio(projected: u64, indexed: u64) -> f64 {
    if indexed == 0 {
        if projected == 0 { 1.0 } else { 0.0 }
    } else {
        projected as f64 / indexed as f64
    }
}

fn read_process_resident_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    if let Some(bytes) = read_linux_vm_rss_bytes() {
        return Some(bytes);
    }

    #[cfg(target_os = "macos")]
    if let Some(bytes) = read_macos_resident_memory_bytes() {
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

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn read_macos_resident_memory_bytes() -> Option<u64> {
    use std::ptr;

    let mut info = libc::mach_task_basic_info {
        virtual_size: 0,
        resident_size: 0,
        resident_size_max: 0,
        user_time: libc::time_value_t {
            seconds: 0,
            microseconds: 0,
        },
        system_time: libc::time_value_t {
            seconds: 0,
            microseconds: 0,
        },
        policy: 0,
        suspend_count: 0,
    };
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;

    // determinism-ok: local task_info call for observability only
    let status = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            ptr::addr_of_mut!(info).cast::<libc::integer_t>(),
            &mut count,
        )
    };

    if status == libc::KERN_SUCCESS {
        Some(info.resident_size)
    } else {
        None
    }
}
