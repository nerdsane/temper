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

pub(crate) mod blob_transport;
mod collection_workflows;
mod handler_deadlines;
mod snapshot_writes;
pub(crate) use collection_workflows::*;
pub use handler_deadlines::*;
pub use snapshot_writes::*;

struct RuntimeMetrics {
    process_resident_memory_bytes: Gauge<u64>,
    active_actors: Gauge<u64>,
    indexed_entities: Gauge<u64>,
    projection_backfill_snapshot_misses_total: Counter<u64>,
    event_replay_duration: Histogram<f64>,
    blob_io_wait_duration_ms: Histogram<f64>,
    blob_local_fast_path_requests_total: Counter<u64>,
    wasm_integration_default_timeout_used_total: Counter<u64>,
    entity_concurrency_retry_total: Counter<u64>,
    entity_concurrency_retry_attempts: Histogram<u64>,
    // --- ADR-0048: dispatch retry + error taxonomy ------------------------
    dispatch_ask_attempts: Histogram<u64>,
    dispatch_ask_latency_ms: Histogram<f64>,
    dispatch_ask_outcome_total: Counter<u64>,
    dispatch_ask_error_total: Counter<u64>,
    // --- ADR-0049: state-entry timeouts -----------------------------------
    state_timeout_fired_total: Counter<u64>,
    state_timeout_cancelled_total: Counter<u64>,
    state_timeout_reset_total: Counter<u64>,
    scheduler_overdue_on_replay_total: Counter<u64>,
    scheduler_pending_timers: Gauge<u64>,
    // ADR-0056: re-arm on actor hydration.
    state_timeout_armed_on_hydration_total: Counter<u64>,
    // ADR-0056 Sub-Decision 3: silent-exit regression guard.
    integration_silent_exit_total: Counter<u64>,
    // ADR-0152: background integration failure that could not be compensated.
    integration_failure_dropped_total: Counter<u64>,
    // --- ADR-0158: durable entity-reaction delivery ----------------------
    reaction_delivery_outcome_total: Counter<u64>,
    reaction_delivery_attempts: Histogram<u64>,
    reaction_delivery_lease_recovered_total: Counter<u64>,
    #[cfg(feature = "observe")]
    reaction_delivery_manual_retry_total: Counter<u64>,
    reaction_delivery_queue_age_ms: Histogram<f64>,
    // --- ADR-0181: bounded collection workflows -------------------------
    collection_workflows: collection_workflows::CollectionWorkflowMetrics,
    // --- ADR-0050: liveness coverage enforcement --------------------------
    spec_liveness_violations_total: Counter<u64>,
    spec_allow_indefinite_states: Gauge<u64>,
    // --- ADR-0051: admission control --------------------------------------
    admission_granted_total: Counter<u64>,
    admission_queued_total: Counter<u64>,
    admission_deferred_total: Counter<u64>,
    admission_wait_time_ms: Histogram<f64>,
    admission_active_permits: Gauge<u64>,
    admission_queue_depth: Gauge<u64>,
    admission_permit_hold_time_ms: Histogram<f64>,
    // --- Actor runtime (mailbox + ask) ------------------------------------
    actor_mailbox_depth: Gauge<u64>,
    actor_mailbox_utilization: Gauge<f64>,
    actor_mailbox_full_drop_total: Counter<u64>,
    actor_ask_reply_latency_ms: Histogram<f64>,
    // --- Dispatch contention (W2 / temper#146) ----------------------------
    actor_registry_lock_wait_ms: Histogram<f64>,
    actor_cold_start_duration_ms: Histogram<f64>,
    event_store_append_wait_ms: Histogram<f64>,
    snapshot_write_started_total: Counter<u64>,
    snapshot_write_error_total: Counter<u64>,
    snapshot_write_coalesced_total: Counter<u64>,
    snapshot_write_stale_skipped_total: Counter<u64>,
    snapshot_write_dropped_total: Counter<u64>,
    snapshot_write_queue_depth: Gauge<u64>,
    snapshot_write_queue_wait_ms: Histogram<f64>,
    snapshot_write_duration_ms: Histogram<f64>,
    snapshot_write_end_to_end_duration_ms: Histogram<f64>,
    snapshot_write_applied_sequence: Gauge<u64>,
    // cedar_eval_duration is emitted from temper-authz (existing
    // temper_cedar_evaluation_duration histogram) — no duplicate here.
    // --- Handler-deadline primitive (W3 / temper#147 — reserved) ---------
    // Names and tag shapes are frozen here so dashboards and monitors can
    // be authored before the primitive lands. Emission sites wire on when
    // the Wasmtime epoch-interruption layer ships.
    handler_deadline_remaining_ms: Gauge<u64>,
    handler_deadline_exceeded_total: Counter<u64>,
    wasm_epoch_tick_interval_ms: Histogram<f64>,
    handler_kill_latency_ms: Histogram<f64>,
    // --- Katagami (consumer-side outcome) ---------------------------------
    curation_job_duration_ms: Histogram<f64>,
    curation_job_outcome_total: Counter<u64>,
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
            dispatch_ask_attempts: meter
                .u64_histogram("temper_dispatch_ask_attempts")
                .with_description("ADR-0048: retry attempts consumed per dispatch call.")
                .build(),
            dispatch_ask_latency_ms: meter
                .f64_histogram("temper_dispatch_ask_latency_ms")
                .with_unit("ms")
                .with_description("ADR-0048: end-to-end dispatch latency including retries.")
                .build(),
            dispatch_ask_outcome_total: meter
                .u64_counter("temper_dispatch_ask_outcome_total")
                .with_description(
                    "ADR-0048: dispatch outcomes — ok, transient_retried_ok, \
                     transient_exhausted, permanent, deferred.",
                )
                .build(),
            dispatch_ask_error_total: meter
                .u64_counter("temper_dispatch_ask_error_total")
                .with_description(
                    "ADR-0048: ActorError kind breakdown (ask_timeout, mailbox_full, \
                     stopped, send_failed, panicked, init_failed, max_restarts_exceeded).",
                )
                .build(),
            state_timeout_fired_total: meter
                .u64_counter("temper_state_timeout_fired_total")
                .with_description("ADR-0049: state_timeout declarations that fired.")
                .build(),
            state_timeout_cancelled_total: meter
                .u64_counter("temper_state_timeout_cancelled_total")
                .with_description("ADR-0049: timers cancelled due to state exit.")
                .build(),
            state_timeout_reset_total: meter
                .u64_counter("temper_state_timeout_reset_total")
                .with_description("ADR-0049: timers re-armed via reset_on.")
                .build(),
            scheduler_overdue_on_replay_total: meter
                .u64_counter("temper_scheduler_overdue_on_replay_total")
                .with_description(
                    "ADR-0049: scheduled actions whose deadline passed before \
                     replay detected them; fired with overdue=true.",
                )
                .build(),
            scheduler_pending_timers: meter
                .u64_gauge("temper_scheduler_pending_timers")
                .with_description("ADR-0049: live in-memory timer count per entity type.")
                .build(),
            state_timeout_armed_on_hydration_total: meter
                .u64_counter("temper_state_timeout_armed_on_hydration_total")
                .with_description(
                    "ADR-0056: timers re-armed when an actor hydrated into a state with a \
                     declared [[state_timeout]] but no live in-memory timer. `elapsed_bucket` \
                     tag distinguishes overdue (fired immediately) from budgeted (armed with \
                     remaining delay).",
                )
                .build(),
            integration_silent_exit_total: meter
                .u64_counter("temper_integration_silent_exit_total")
                .with_description(
                    "ADR-0056: inline WASM trigger-integration invocations that returned \
                     successfully without causing any state transition on the triggering \
                     entity. Under healthy operation this counter is permanently zero; any \
                     nonzero reading is a regression of the consumer-side \
                     exit-dispatches-an-action invariant (openpaw ADR-0039 Sub-Decision 3a) \
                     or a transient persist failure that slipped past the retry layer \
                     (ADR-0056 Sub-Decision 2). Alerts on this counter are critical-severity.",
                )
                .build(),
            integration_failure_dropped_total: meter
                .u64_counter("temper_integration_failure_dropped_total")
                .with_description(
                    "ADR-0152: a background integration failed with no declared `on_failure` \
                     and the source entity had no enabled `Fail`/error transition to \
                     compensate it. The failure could not be turned into a state change, so it \
                     is surfaced here (and as an `integration_failure_dropped` Observe event) \
                     instead of being silently dropped. Any nonzero reading means an entity's \
                     spec lacks a failure path for an integration that can fail — \
                     critical-severity.",
                )
                .build(),
            reaction_delivery_outcome_total: meter
                .u64_counter("temper_reaction_delivery_outcome_total")
                .with_description("ADR-0158: durable reaction delivery outcomes.")
                .build(),
            reaction_delivery_attempts: meter
                .u64_histogram("temper_reaction_delivery_attempts")
                .with_description("ADR-0158: automatic attempts consumed by a durable reaction delivery.")
                .build(),
            reaction_delivery_lease_recovered_total: meter
                .u64_counter("temper_reaction_delivery_lease_recovered_total")
                .with_description("ADR-0158: expired durable reaction leases recovered after interruption.")
                .build(),
            #[cfg(feature = "observe")]
            reaction_delivery_manual_retry_total: meter
                .u64_counter("temper_reaction_delivery_manual_retry_total")
                .with_description("ADR-0158: operator retry requests, classified by outcome.")
                .build(),
            reaction_delivery_queue_age_ms: meter
                .f64_histogram("temper_reaction_delivery_queue_age_ms")
                .with_unit("ms")
                .with_description("ADR-0158: age of a durable reaction delivery at terminal outcome.")
                .build(),
            collection_workflows: collection_workflows::CollectionWorkflowMetrics::new(&meter),
            spec_liveness_violations_total: meter
                .u64_counter("temper_spec_liveness_violations_total")
                .with_description(
                    "ADR-0050: non-terminal states found without [[state_timeout]] \
                     or allow_indefinite_states coverage.",
                )
                .build(),
            spec_allow_indefinite_states: meter
                .u64_gauge("temper_spec_allow_indefinite_states")
                .with_description("ADR-0050: explicitly allowlisted indefinite states per entity.")
                .build(),
            admission_granted_total: meter
                .u64_counter("temper_admission_granted_total")
                .with_description("ADR-0051: permits granted immediately or after queueing.")
                .build(),
            admission_queued_total: meter
                .u64_counter("temper_admission_queued_total")
                .with_description("ADR-0051: acquirers that had to wait before being granted.")
                .build(),
            admission_deferred_total: meter
                .u64_counter("temper_admission_deferred_total")
                .with_description("ADR-0051: acquirers that hit queue_timeout_seconds.")
                .build(),
            admission_wait_time_ms: meter
                .f64_histogram("temper_admission_wait_time_ms")
                .with_unit("ms")
                .with_description("ADR-0051: time spent waiting in the admission queue.")
                .build(),
            admission_active_permits: meter
                .u64_gauge("temper_admission_active_permits")
                .with_description("ADR-0051: permits currently held per (tenant, entity, action).")
                .build(),
            admission_queue_depth: meter
                .u64_gauge("temper_admission_queue_depth")
                .with_description("ADR-0051: pending acquirers per (tenant, entity, action).")
                .build(),
            admission_permit_hold_time_ms: meter
                .f64_histogram("temper_admission_permit_hold_time_ms")
                .with_unit("ms")
                .with_description("ADR-0051: duration permits were held before release.")
                .build(),
            actor_mailbox_depth: meter
                .u64_gauge("temper_actor_mailbox_depth")
                .with_description("Per-actor instantaneous mailbox queue depth.")
                .build(),
            actor_mailbox_utilization: meter
                .f64_gauge("temper_actor_mailbox_utilization")
                .with_description("Per-entity-type aggregate mailbox utilization [0.0..1.0].")
                .build(),
            actor_mailbox_full_drop_total: meter
                .u64_counter("temper_actor_mailbox_full_drop_total")
                .with_description(
                    "Real MailboxFull occurrences. Drives ADR-0048 retry — and ADR-0051 \
                     admission is supposed to suppress it.",
                )
                .build(),
            actor_ask_reply_latency_ms: meter
                .f64_histogram("temper_actor_ask_reply_latency_ms")
                .with_unit("ms")
                .with_description("Inside-actor ask handling latency (excludes dispatch overhead).")
                .build(),
            actor_registry_lock_wait_ms: meter
                .f64_histogram("temper_actor_registry_lock_wait_ms")
                .with_unit("ms")
                .with_description(
                    "Wall-clock time between actor-registry lookup request and grant. \
                     Drives temper#146 investigation: under bursty cold-start load, high \
                     p95 here points at the registry mutex as the bottleneck.",
                )
                .build(),
            actor_cold_start_duration_ms: meter
                .f64_histogram("temper_actor_cold_start_duration_ms")
                .with_unit("ms")
                .with_description(
                    "End-to-end duration from first-message arrival to first-reply-ready \
                     for a previously unhydrated actor. See temper#146.",
                )
                .build(),
            event_store_append_wait_ms: meter
                .f64_histogram("temper_event_store_append_wait_ms")
                .with_unit("ms")
                .with_description(
                    "Time between event-store append() call and return, including any \
                     writer-lock or fsync serialization. High p95 points at storage as a \
                     cold-start bottleneck. See temper#146.",
                )
                .build(),
            snapshot_write_started_total: meter
                .u64_counter("temper_snapshot_write_started_total")
                .with_description("Queued snapshot writes started by the background worker.")
                .build(),
            snapshot_write_error_total: meter
                .u64_counter("temper_snapshot_write_error_total")
                .with_description("Queued snapshot writes that failed in the event store.")
                .build(),
            snapshot_write_coalesced_total: meter
                .u64_counter("temper_snapshot_write_coalesced_total")
                .with_description(
                    "Snapshot enqueue attempts coalesced behind a newer pending write for the same stream.",
                )
                .build(),
            snapshot_write_stale_skipped_total: meter
                .u64_counter("temper_snapshot_write_stale_skipped_total")
                .with_description(
                    "Snapshot enqueue attempts skipped before storage because a same-stream newer sequence was already pending.",
                )
                .build(),
            snapshot_write_dropped_total: meter
                .u64_counter("temper_snapshot_write_dropped_total")
                .with_description(
                    "Snapshot enqueue attempts rejected because the bounded queue was full.",
                )
                .build(),
            snapshot_write_queue_depth: meter
                .u64_gauge("temper_snapshot_write_queue_depth")
                .with_description("Pending queued snapshot writes after enqueue or drain.")
                .build(),
            snapshot_write_queue_wait_ms: meter
                .f64_histogram("temper_snapshot_write_queue_wait_ms")
                .with_unit("ms")
                .with_description("Time a snapshot write spent queued before storage began.")
                .build(),
            snapshot_write_duration_ms: meter
                .f64_histogram("temper_snapshot_write_duration_ms")
                .with_unit("ms")
                .with_description("Storage duration for a queued snapshot write.")
                .build(),
            snapshot_write_end_to_end_duration_ms: meter
                .f64_histogram("temper_snapshot_write_end_to_end_duration_ms")
                .with_unit("ms")
                .with_description("End-to-end snapshot lag from enqueue to write completion.")
                .build(),
            snapshot_write_applied_sequence: meter
                .u64_gauge("temper_snapshot_write_applied_sequence")
                .with_description("Latest snapshot sequence successfully written by the queue.")
                .build(),
            handler_deadline_remaining_ms: meter
                .u64_gauge("temper_handler_deadline_remaining_ms")
                .with_unit("ms")
                .with_description(
                    "W3 reserved (temper#147): budget remaining at WASM dispatch start. \
                     Gauges how tight current deadlines are relative to observed \
                     handler latency. Emitted when the handler-deadline primitive \
                     lands.",
                )
                .build(),
            handler_deadline_exceeded_total: meter
                .u64_counter("temper_handler_deadline_exceeded_total")
                .with_description(
                    "W3 reserved (temper#147): WASM handlers killed for exceeding their \
                     deadline. Tagged with `dying_span` (which host function was running \
                     when the guest was killed); without that tag the metric would be \
                     uninvestigatable.",
                )
                .build(),
            wasm_epoch_tick_interval_ms: meter
                .f64_histogram("temper_wasm_epoch_tick_interval_ms")
                .with_unit("ms")
                .with_description(
                    "W3 reserved (temper#147): Wasmtime epoch-interruption ticker \
                     interval. Drift here makes deadlines imprecise.",
                )
                .build(),
            handler_kill_latency_ms: meter
                .f64_histogram("temper_handler_kill_latency_ms")
                .with_unit("ms")
                .with_description(
                    "W3 reserved (temper#147): time from deadline breach to guest \
                     actually exiting. Detects guest code that resists termination.",
                )
                .build(),
            curation_job_duration_ms: meter
                .f64_histogram("temper_curation_job_duration_ms")
                .with_unit("ms")
                .with_description("End-to-end CurationJob latency (Katagami).")
                .build(),
            curation_job_outcome_total: meter
                .u64_counter("temper_curation_job_outcome_total")
                .with_description("CurationJob outcomes — completed, failed, deferred.")
                .build(),
        }
    })
}

/// Record a terminal durable-reaction outcome without high-cardinality IDs.
pub(crate) fn record_reaction_delivery_outcome(
    kind: &'static str,
    outcome: &'static str,
    attempts: u32,
    queue_age: Duration,
) {
    let attrs = [
        KeyValue::new("kind", kind),
        KeyValue::new("outcome", outcome),
    ];
    metrics().reaction_delivery_outcome_total.add(1, &attrs);
    metrics()
        .reaction_delivery_attempts
        .record(u64::from(attempts), &attrs);
    metrics()
        .reaction_delivery_queue_age_ms
        .record(queue_age.as_secs_f64() * 1000.0, &attrs);
}

/// Record a low-cardinality durable-reaction lifecycle event.
pub(crate) fn record_reaction_delivery_event(kind: &'static str, event: &'static str) {
    metrics().reaction_delivery_outcome_total.add(
        1,
        &[KeyValue::new("kind", kind), KeyValue::new("outcome", event)],
    );
}

/// Record recovery of an expired fenced delivery lease.
pub(crate) fn record_reaction_delivery_lease_recovered(kind: &'static str) {
    metrics()
        .reaction_delivery_lease_recovered_total
        .add(1, &[KeyValue::new("kind", kind)]);
}

/// Record an operator retry decision without identifying the delivery.
#[cfg(feature = "observe")]
pub(crate) fn record_reaction_delivery_manual_retry(outcome: &'static str) {
    metrics()
        .reaction_delivery_manual_retry_total
        .add(1, &[KeyValue::new("outcome", outcome)]);
}

/// Record actor and entity counts from the current server state snapshot.
pub fn record_server_state_metrics(state: &ServerState) {
    if let Ok(registry) = state.actor_registry.read() {
        record_active_actor_count(registry.len());
        record_actor_mailbox_metrics(&registry);
    }
    if let Ok(index) = state.entity_index.read() {
        record_active_entity_counts(&index);
    }
}

/// Walk the actor registry and emit per-actor mailbox depth + per-entity-type
/// aggregate utilization gauges (ADR-0048 observability baseline).
fn record_actor_mailbox_metrics(
    registry: &BTreeMap<String, temper_runtime::actor::ActorRef<crate::entity_actor::EntityMsg>>,
) {
    // actor_key format is "{tenant}:{entity_type}:{entity_id}"; we aggregate
    // utilization per entity type and sample individual depth per actor.
    let mut per_type_total_util: BTreeMap<String, (f64, u64)> = BTreeMap::new();
    for (key, actor_ref) in registry.iter() {
        let entity_type = key
            .split(':')
            .nth(1)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "_unknown_".to_string());
        record_actor_mailbox_depth(&entity_type, key, actor_ref.mailbox_depth() as u64);
        let entry = per_type_total_util.entry(entity_type).or_insert((0.0, 0));
        entry.0 += actor_ref.mailbox_utilization();
        entry.1 += 1;
    }
    for (entity_type, (sum_util, count)) in per_type_total_util {
        if count > 0 {
            record_actor_mailbox_utilization(&entity_type, sum_util / count as f64);
        }
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

// ============================================================================
// ADR-0048: dispatch retry + error taxonomy.
// ============================================================================

/// Dispatch outcome classifications surfaced as the `outcome` metric label.
#[derive(Debug, Clone, Copy)]
pub enum DispatchOutcome {
    Ok,
    TransientRetriedOk,
    TransientExhausted,
    Permanent,
    Deferred,
}

impl DispatchOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::TransientRetriedOk => "transient_retried_ok",
            Self::TransientExhausted => "transient_exhausted",
            Self::Permanent => "permanent",
            Self::Deferred => "deferred",
        }
    }
}

/// Record the outcome of a dispatch call along with attempt count and
/// elapsed latency.
pub fn record_dispatch_outcome(
    tenant: &str,
    entity_type: &str,
    action: &str,
    outcome: DispatchOutcome,
    attempts: u32,
    elapsed: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("action", action.to_string()),
        KeyValue::new("outcome", outcome.as_str()),
    ];
    metrics().dispatch_ask_outcome_total.add(1, &attrs);
    metrics()
        .dispatch_ask_attempts
        .record(attempts as u64, &attrs);
    metrics()
        .dispatch_ask_latency_ms
        .record(elapsed.as_secs_f64() * 1000.0, &attrs);
}

/// Record a specific ActorError variant that a dispatch attempt surfaced.
pub fn record_dispatch_error(
    tenant: &str,
    entity_type: &str,
    action: &str,
    error_kind: &'static str,
) {
    metrics().dispatch_ask_error_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
            KeyValue::new("error_kind", error_kind),
        ],
    );
}

// ============================================================================
// ADR-0049: state-entry timeouts.
// ============================================================================

/// Record that a state_timeout fired for a specific (entity, state, action).
pub fn record_state_timeout_fired(tenant: &str, entity_type: &str, state: &str, action: &str) {
    metrics().state_timeout_fired_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("state", state.to_string()),
            KeyValue::new("action", action.to_string()),
        ],
    );
}

/// Record a state_timeout cancellation triggered by a state exit.
pub fn record_state_timeout_cancelled(tenant: &str, entity_type: &str, state: &str) {
    metrics().state_timeout_cancelled_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("state", state.to_string()),
        ],
    );
}

/// Record a state_timeout re-arm triggered by a reset_on action.
pub fn record_state_timeout_reset(
    tenant: &str,
    entity_type: &str,
    state: &str,
    reset_action: &str,
) {
    metrics().state_timeout_reset_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("state", state.to_string()),
            KeyValue::new("reset_action", reset_action.to_string()),
        ],
    );
}

/// Record a state_timeout re-armed by the hydration hook (ADR-0056). `bucket`
/// is `"overdue"` when the timer fired immediately because elapsed >=
/// after_seconds, or `"budgeted"` when armed with remaining delay.
pub fn record_state_timeout_armed_on_hydration(
    tenant: &str,
    entity_type: &str,
    state: &str,
    bucket: &'static str,
) {
    metrics().state_timeout_armed_on_hydration_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("state", state.to_string()),
            KeyValue::new("elapsed_bucket", bucket),
        ],
    );
}

/// Record a silent integration exit — an inline trigger invocation that
/// returned successfully without causing a state transition on the
/// triggering entity. Emits `temper_integration_silent_exit_total` (ADR-0056
/// Sub-Decision 3).
pub fn record_integration_silent_exit(
    tenant: &str,
    entity_type: &str,
    triggering_action: &str,
    state: &str,
) {
    metrics().integration_silent_exit_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", triggering_action.to_string()),
            KeyValue::new("state", state.to_string()),
        ],
    );
}

/// Record a background integration failure that could not be compensated
/// (ADR-0152): the integration failed with no `on_failure`, and the source
/// entity had no enabled `Fail`/error transition. Emits
/// `temper_integration_failure_dropped_total`. This counter should be zero in a
/// healthy system; any reading is a critical-severity alert that an entity's
/// spec lacks a failure path.
pub fn record_integration_failure_dropped(
    tenant: &str,
    entity_type: &str,
    triggering_action: &str,
    state: &str,
) {
    metrics().integration_failure_dropped_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", triggering_action.to_string()),
            KeyValue::new("state", state.to_string()),
        ],
    );
}

/// Record a timer that missed its deadline across a restart and had to be
/// re-fired with `overdue=true`.
pub fn record_scheduler_overdue_on_replay(tenant: &str, entity_type: &str) {
    metrics().scheduler_overdue_on_replay_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
        ],
    );
}

/// Report the current in-memory pending-timer count per entity type.
pub fn record_scheduler_pending_timers(entity_type: &str, count: u64) {
    metrics().scheduler_pending_timers.record(
        count,
        &[KeyValue::new("entity_type", entity_type.to_string())],
    );
}

// ============================================================================
// ADR-0050: liveness coverage enforcement.
// ============================================================================

pub fn record_spec_liveness_violation(entity_type: &str, state: &str) {
    metrics().spec_liveness_violations_total.add(
        1,
        &[
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("state", state.to_string()),
        ],
    );
}

pub fn record_spec_allow_indefinite_states(entity_type: &str, count: u64) {
    metrics().spec_allow_indefinite_states.record(
        count,
        &[KeyValue::new("entity_type", entity_type.to_string())],
    );
}

// ============================================================================
// ADR-0051: admission control.
// ============================================================================

pub fn record_admission_granted(tenant: &str, entity_type: &str, action: &str, waited: Duration) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("action", action.to_string()),
    ];
    metrics().admission_granted_total.add(1, &attrs);
    metrics()
        .admission_wait_time_ms
        .record(waited.as_secs_f64() * 1000.0, &attrs);
}

pub fn record_admission_queued(tenant: &str, entity_type: &str, action: &str) {
    metrics().admission_queued_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
        ],
    );
}

pub fn record_admission_deferred(tenant: &str, entity_type: &str, action: &str, waited: Duration) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("action", action.to_string()),
    ];
    metrics().admission_deferred_total.add(1, &attrs);
    metrics()
        .admission_wait_time_ms
        .record(waited.as_secs_f64() * 1000.0, &attrs);
}

pub fn record_admission_active_permits(tenant: &str, entity_type: &str, action: &str, count: u64) {
    metrics().admission_active_permits.record(
        count,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
        ],
    );
}

pub fn record_admission_queue_depth(tenant: &str, entity_type: &str, action: &str, depth: u64) {
    metrics().admission_queue_depth.record(
        depth,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
        ],
    );
}

pub fn record_admission_permit_hold(tenant: &str, entity_type: &str, action: &str, held: Duration) {
    metrics().admission_permit_hold_time_ms.record(
        held.as_secs_f64() * 1000.0,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
        ],
    );
}

// ============================================================================
// Actor runtime (mailbox + ask).
// ============================================================================

/// Bucket an actor id hash into a low-cardinality slot so per-actor gauges
/// stay within Datadog cardinality limits.
pub fn actor_id_bucket(actor_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    actor_id.hash(&mut h);
    format!("{:02x}", h.finish() % 64)
}

pub fn record_actor_mailbox_depth(entity_type: &str, actor_id: &str, depth: u64) {
    metrics().actor_mailbox_depth.record(
        depth,
        &[
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("actor_id_hash", actor_id_bucket(actor_id)),
        ],
    );
}

pub fn record_actor_mailbox_utilization(entity_type: &str, utilization: f64) {
    metrics().actor_mailbox_utilization.record(
        utilization,
        &[KeyValue::new("entity_type", entity_type.to_string())],
    );
}

pub fn record_actor_mailbox_full_drop(entity_type: &str, action: &str) {
    metrics().actor_mailbox_full_drop_total.add(
        1,
        &[
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
        ],
    );
}

pub fn record_actor_ask_reply_latency(entity_type: &str, action: &str, elapsed: Duration) {
    metrics().actor_ask_reply_latency_ms.record(
        elapsed.as_secs_f64() * 1000.0,
        &[
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
        ],
    );
}

/// Record time spent in actor-registry lookup / spawn. High p95 points at
/// mutex contention on the registry — core signal for temper#146.
pub fn record_actor_registry_lock_wait(entity_type: &str, was_cold_start: bool, elapsed: Duration) {
    metrics().actor_registry_lock_wait_ms.record(
        elapsed.as_secs_f64() * 1000.0,
        &[
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("cold_start", was_cold_start),
        ],
    );
}

/// Record end-to-end cold-start duration for a freshly spawned actor.
pub fn record_actor_cold_start_duration(entity_type: &str, elapsed: Duration) {
    metrics().actor_cold_start_duration_ms.record(
        elapsed.as_secs_f64() * 1000.0,
        &[KeyValue::new("entity_type", entity_type.to_string())],
    );
}

/// Record time between event-store append() call and return.
pub fn record_event_store_append_wait(backend: &str, operation: &str, elapsed: Duration) {
    metrics().event_store_append_wait_ms.record(
        elapsed.as_secs_f64() * 1000.0,
        &[
            KeyValue::new("backend", backend.to_string()),
            KeyValue::new("operation", operation.to_string()),
        ],
    );
}

// Cedar evaluation duration is emitted from the temper-authz crate via the
// existing `record_cedar_evaluation` helper. No duplicate metric here.

// ============================================================================
// Katagami.
// ============================================================================

pub fn record_curation_job_duration(job_type: &str, duration: Duration) {
    metrics().curation_job_duration_ms.record(
        duration.as_secs_f64() * 1000.0,
        &[KeyValue::new("job_type", job_type.to_string())],
    );
}

pub fn record_curation_job_outcome(job_type: &str, outcome: &'static str) {
    metrics().curation_job_outcome_total.add(
        1,
        &[
            KeyValue::new("job_type", job_type.to_string()),
            KeyValue::new("outcome", outcome),
        ],
    );
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
