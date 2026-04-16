//! Runtime metrics exported via OpenTelemetry.
//!
//! This module provides metric recording helpers called from hot paths
//! (entity actor replay, entity ops).  The periodic canary loop and
//! sampler live in `state::runtime_metrics` via `spawn_runtime_metrics_loop`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Gauge, Histogram};
use opentelemetry::{KeyValue, global};

use crate::state::ServerState;

struct RuntimeMetrics {
    process_resident_memory_bytes: Gauge<u64>,
    active_actors: Gauge<u64>,
    indexed_entities: Gauge<u64>,
    projection_backfill_snapshot_misses_total: Counter<u64>,
    event_replay_duration: Histogram<f64>,
    blob_io_wait_duration_ms: Histogram<f64>,
    blob_local_fast_path_requests_total: Counter<u64>,
    monty_repl_acquisitions_total: Counter<u64>,
    monty_repl_observed_active_invocations: Histogram<f64>,
    monty_repl_wait_duration_ms: Histogram<f64>,
    wasm_integration_default_timeout_used_total: Counter<u64>,
    entity_concurrency_retry_total: Counter<u64>,
    entity_concurrency_retry_attempts: Histogram<u64>,
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
            indexed_entities: meter
                .u64_gauge("temper_indexed_entities")
                .with_description(
                    "Number of entities currently present in the in-memory query-plane index, reported globally and by tenant.",
                )
                .build(),
            projection_backfill_snapshot_misses_total: meter
                .u64_counter("temper_projection_backfill_snapshot_misses_total")
                .with_description(
                    "Entities encountered during projection backfill that had no snapshot and required direct event replay.",
                )
                .build(),
            event_replay_duration: meter
                .f64_histogram("temper_event_replay_duration")
                .with_description("Time spent replaying event journals.")
                .build(),
            blob_io_wait_duration_ms: meter
                .f64_histogram("temper_blob_io_wait_duration_ms")
                .with_unit("ms")
                .with_description("Time spent waiting for blob I/O backpressure permits.")
                .build(),
            blob_local_fast_path_requests_total: meter
                .u64_counter("temper_blob_local_fast_path_requests_total")
                .with_description(
                    "Requests served by the in-process local blob fast path without loopback HTTP.",
                )
                .build(),
            monty_repl_acquisitions_total: meter
                .u64_counter("temper_monty_repl_acquisitions_total")
                .with_description("Total number of monty_repl execution gate acquisitions.")
                .build(),
            monty_repl_observed_active_invocations: meter
                .f64_histogram("temper_monty_repl_observed_active_invocations")
                .with_description(
                    "Observed number of concurrent monty_repl WASM executions at acquire/release points.",
                )
                .build(),
            monty_repl_wait_duration_ms: meter
                .f64_histogram("temper_monty_repl_wait_duration_ms")
                .with_unit("ms")
                .with_description("Time spent waiting to acquire the monty_repl execution gate.")
                .build(),
            wasm_integration_default_timeout_used_total: meter
                .u64_counter("temper_wasm_integration_default_timeout_used_total")
                .with_description(
                    "WASM integration dispatches that fell back to the default timeout because the \
                     spec did not set `timeout_secs`. Apps firing this frequently should wire an \
                     explicit timeout in their integration config. See ADR-0045.",
                )
                .build(),
            entity_concurrency_retry_total: meter
                .u64_counter("temper_entity_concurrency_retry_total")
                .with_description(
                    "Entity-actor persist attempts that hit an optimistic concurrency conflict \
                     and either recovered, exhausted the retry budget, or found the action no \
                     longer legal after replay. Treat sustained activity as a canary for an \
                     unknown scheduler race — not a retry budget to raise. See ADR-0046.",
                )
                .build(),
            entity_concurrency_retry_attempts: meter
                .u64_histogram("temper_entity_concurrency_retry_attempts")
                .with_description(
                    "Number of persist attempts a single action consumed before success or \
                     exhaustion. Value of 1 is the no-retry happy path; anything higher is a \
                     canary. See ADR-0046.",
                )
                .build(),
        }
    })
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
    metrics().indexed_entities.record(total, &[]);

    for (tenant, count) in by_tenant {
        metrics()
            .indexed_entities
            .record(count, &[KeyValue::new("tenant", tenant)]);
    }
}

/// Record entities that required replay because no snapshot existed during startup backfill.
pub fn record_projection_backfill_snapshot_misses(tenant: &str, count: u64) {
    metrics()
        .projection_backfill_snapshot_misses_total
        .add(count, &[KeyValue::new("tenant", tenant.to_string())]);
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

/// Record time spent waiting for blob I/O backpressure permits.
pub fn record_blob_io_wait_duration(duration: Duration, operation: &str) {
    metrics().blob_io_wait_duration_ms.record(
        duration.as_secs_f64() * 1000.0,
        &[KeyValue::new("operation", operation.to_string())],
    );
}

/// Record usage of the in-process local blob fast path.
pub fn record_blob_local_fast_path_request(method: &str) {
    metrics()
        .blob_local_fast_path_requests_total
        .add(1, &[KeyValue::new("method", method.to_string())]);
}

/// Record process resident memory usage.
pub fn record_process_resident_memory_bytes(bytes: u64) {
    metrics().process_resident_memory_bytes.record(bytes, &[]);
}

/// Record the number of concurrent monty_repl executions.
pub fn record_monty_repl_active_invocations(count: u64, max_concurrency: usize) {
    metrics().monty_repl_observed_active_invocations.record(
        count as f64,
        &[KeyValue::new(
            "max_concurrency",
            i64::try_from(max_concurrency).unwrap_or(i64::MAX),
        )],
    );
}

/// Record a successful monty_repl gate acquisition.
pub fn record_monty_repl_acquisition(max_concurrency: usize) {
    metrics().monty_repl_acquisitions_total.add(
        1,
        &[KeyValue::new(
            "max_concurrency",
            i64::try_from(max_concurrency).unwrap_or(i64::MAX),
        )],
    );
}

/// Record time spent waiting for the monty_repl execution gate.
pub fn record_monty_repl_wait_duration(duration: Duration, max_concurrency: usize) {
    metrics().monty_repl_wait_duration_ms.record(
        duration.as_secs_f64() * 1000.0,
        &[KeyValue::new(
            "max_concurrency",
            i64::try_from(max_concurrency).unwrap_or(i64::MAX),
        )],
    );
}

/// Record a WASM integration dispatch that fell back to the default timeout
/// because the integration spec did not set `timeout_secs`.
///
/// See ADR-0045.
pub fn record_wasm_default_timeout_used(tenant: &str, entity_type: &str, module: &str) {
    metrics().wasm_integration_default_timeout_used_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("module", module.to_string()),
        ],
    );
}

/// Possible outcomes for an entity-actor concurrency retry cycle.
///
/// See ADR-0046.
#[derive(Debug, Clone, Copy)]
pub enum ConcurrencyRetryOutcome {
    /// The action persisted successfully (possibly after one or more retries).
    Success,
    /// All retry attempts hit `ConcurrencyViolation`; the action was dropped.
    Exhausted,
    /// Replay caught the entity in a state where the action is no longer legal
    /// (e.g., the entity reached a terminal state during the race).
    ActionIllegal,
}

impl ConcurrencyRetryOutcome {
    /// Short string identifier for metric labels and span attributes.
    pub fn as_str(&self) -> &'static str {
        match self {
            ConcurrencyRetryOutcome::Success => "success",
            ConcurrencyRetryOutcome::Exhausted => "exhausted",
            ConcurrencyRetryOutcome::ActionIllegal => "action_illegal",
        }
    }
}

/// Record the outcome of an entity-actor concurrency retry cycle plus the
/// number of attempts consumed. Attempts is 1-based (1 = no retries).
///
/// See ADR-0046.
pub fn record_entity_concurrency_retry(
    entity_type: &str,
    outcome: ConcurrencyRetryOutcome,
    attempts: u64,
) {
    let attrs = [
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("outcome", outcome.as_str()),
    ];
    metrics().entity_concurrency_retry_total.add(1, &attrs);
    metrics()
        .entity_concurrency_retry_attempts
        .record(attempts, &attrs);
}

/// Read process resident memory (RSS) in bytes from Linux procfs.
#[cfg(target_os = "linux")]
pub fn read_process_resident_memory_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?; // determinism-ok: procfs RSS read for observability only
    let vm_rss_line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let mut parts = vm_rss_line.split_whitespace();
    let _label = parts.next()?;
    let value_kb = parts.next()?.parse::<u64>().ok()?;
    Some(value_kb.saturating_mul(1024))
}

/// Read process resident memory (RSS) in bytes from Linux procfs.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub fn read_process_resident_memory_bytes() -> Option<u64> {
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read_process_resident_memory_bytes() -> Option<u64> {
    None
}
