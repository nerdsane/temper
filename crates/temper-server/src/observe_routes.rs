//! Observe API routes for developer tooling.
//!
//! These endpoints expose internal Temper state for the observability frontend.
//! They are only available when the `observe` feature is enabled.

use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::sim_now;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use temper_evolution::{
    Decision, DecisionRecord, RecordHeader, RecordStatus, RecordType,
    validate_chain,
};

use crate::dispatch::extract_tenant;
use crate::entity_actor::{EntityEvent, EntityMsg, EntityResponse};
use crate::sentinel;
use crate::state::{ServerState, TrajectoryEntry};

/// Summary of a loaded spec.
#[derive(Serialize, Deserialize)]
pub struct SpecSummary {
    /// Tenant that owns this spec.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Valid status states.
    pub states: Vec<String>,
    /// Action names.
    pub actions: Vec<String>,
    /// Initial state.
    pub initial_state: String,
}

/// Full spec detail.
#[derive(Serialize, Deserialize)]
pub struct SpecDetail {
    /// Entity type name.
    pub entity_type: String,
    /// Valid status states.
    pub states: Vec<String>,
    /// Initial state.
    pub initial_state: String,
    /// Action details.
    pub actions: Vec<ActionDetail>,
    /// Invariant details.
    pub invariants: Vec<InvariantDetail>,
    /// State variable declarations.
    pub state_variables: Vec<StateVarDetail>,
}

/// Detail of a single action.
#[derive(Serialize, Deserialize)]
pub struct ActionDetail {
    /// Action name.
    pub name: String,
    /// Action kind (input/output/internal).
    pub kind: String,
    /// States from which this action can fire.
    pub from: Vec<String>,
    /// Target state after firing.
    pub to: Option<String>,
    /// Guard conditions (Debug representation).
    pub guards: Vec<String>,
    /// Effects (Debug representation).
    pub effects: Vec<String>,
}

/// Detail of a single invariant.
#[derive(Serialize, Deserialize)]
pub struct InvariantDetail {
    /// Invariant name.
    pub name: String,
    /// Trigger states (empty = always checked).
    pub when: Vec<String>,
    /// Assertion expression.
    pub assertion: String,
}

/// Detail of a state variable.
#[derive(Serialize, Deserialize)]
pub struct StateVarDetail {
    /// Variable name.
    pub name: String,
    /// Variable type.
    pub var_type: String,
    /// Initial value.
    pub initial: String,
}

/// Entity instance summary.
#[derive(Serialize, Deserialize)]
pub struct EntityInstanceSummary {
    /// Entity type.
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Actor liveness status (e.g. "active", "stopped").
    pub actor_status: String,
}

/// Query parameters for the simulation endpoint.
#[derive(Deserialize)]
pub struct SimQueryParams {
    /// PRNG seed (default: 42).
    pub seed: Option<u64>,
    /// Max simulation ticks (default: 200).
    pub ticks: Option<u64>,
}

/// Query parameters for the SSE event stream.
#[derive(Deserialize)]
pub struct EventStreamParams {
    /// Filter by entity type.
    pub entity_type: Option<String>,
    /// Filter by entity ID.
    pub entity_id: Option<String>,
}

/// Build the observe router (mounted at /observe).
pub fn build_observe_router() -> Router<ServerState> {
    Router::new()
        .route("/specs", get(list_specs))
        .route("/specs/{entity}", get(get_spec_detail))
        .route("/entities", get(list_entities))
        .route("/verify/{entity}", post(run_verification))
        .route("/simulation/{entity}", get(run_simulation))
        .route("/entities/{entity_type}/{entity_id}/history", get(get_entity_history))
        .route("/events/stream", get(handle_event_stream))
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route("/trajectories", get(handle_trajectories))
        .route("/sentinel/check", post(handle_sentinel_check))
        .route("/evolution/records", get(list_evolution_records))
        .route("/evolution/records/{id}", get(get_evolution_record))
        .route("/evolution/records/{id}/decide", post(handle_decide))
        .route("/evolution/insights", get(list_evolution_insights))
}

/// GET /observe/specs -- list all loaded specs across all tenants.
async fn list_specs(State(state): State<ServerState>) -> Json<Vec<SpecSummary>> {
    let registry = state.registry.read().unwrap();
    let mut specs = Vec::new();

    for tenant_id in registry.tenant_ids() {
        for entity_type in registry.entity_types(tenant_id) {
            if let Some(entity_spec) = registry.get_spec(tenant_id, entity_type) {
                let automaton = &entity_spec.automaton;
                specs.push(SpecSummary {
                    tenant: tenant_id.as_str().to_string(),
                    entity_type: entity_type.to_string(),
                    states: automaton.automaton.states.clone(),
                    actions: automaton.actions.iter().map(|a| a.name.clone()).collect(),
                    initial_state: automaton.automaton.initial.clone(),
                });
            }
        }
    }

    Json(specs)
}

/// GET /observe/specs/{entity} -- full spec detail for a named entity type.
///
/// Searches across all tenants and returns the first match.
async fn get_spec_detail(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
) -> Result<Json<SpecDetail>, StatusCode> {
    let registry = state.registry.read().unwrap();

    for tenant_id in registry.tenant_ids() {
        if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
            let automaton = &entity_spec.automaton;
            let detail = SpecDetail {
                entity_type: entity.clone(),
                states: automaton.automaton.states.clone(),
                initial_state: automaton.automaton.initial.clone(),
                actions: automaton
                    .actions
                    .iter()
                    .map(|a| ActionDetail {
                        name: a.name.clone(),
                        kind: a.kind.clone(),
                        from: a.from.clone(),
                        to: a.to.clone(),
                        guards: a.guard.iter().map(|g| format!("{g:?}")).collect(),
                        effects: a.effect.iter().map(|e| format!("{e:?}")).collect(),
                    })
                    .collect(),
                invariants: automaton
                    .invariants
                    .iter()
                    .map(|i| InvariantDetail {
                        name: i.name.clone(),
                        when: i.when.clone(),
                        assertion: i.assert.clone(),
                    })
                    .collect(),
                state_variables: automaton
                    .state
                    .iter()
                    .map(|sv| StateVarDetail {
                        name: sv.name.clone(),
                        var_type: sv.var_type.clone(),
                        initial: sv.initial.clone(),
                    })
                    .collect(),
            };
            return Ok(Json(detail));
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// GET /observe/entities -- list active entity instances from the actor registry.
async fn list_entities(State(state): State<ServerState>) -> Json<Vec<EntityInstanceSummary>> {
    let registry = state.actor_registry.read().unwrap();
    let entities: Vec<EntityInstanceSummary> = registry
        .keys()
        .map(|key| {
            // Actor keys are formatted as "{tenant}:{entity_type}:{entity_id}"
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            EntityInstanceSummary {
                entity_type: parts.get(1).unwrap_or(&"unknown").to_string(),
                entity_id: parts.get(2).unwrap_or(&"unknown").to_string(),
                actor_status: "active".to_string(),
            }
        })
        .collect();
    Json(entities)
}

/// POST /observe/verify/{entity} -- run verification cascade on a spec.
///
/// Runs all levels (L0 SMT, L1 Model Check, L2 DST, L3 PropTest) and returns results.
async fn run_verification(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
) -> Result<Json<temper_verify::CascadeResult>, StatusCode> {
    let ioa_source = {
        let registry = state.registry.read().unwrap();
        let mut found = None;
        for tenant_id in registry.tenant_ids() {
            if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
                found = Some(entity_spec.ioa_source.clone());
                break;
            }
        }
        found
    };

    let Some(ioa_source) = ioa_source else {
        return Err(StatusCode::NOT_FOUND);
    };

    // Run the cascade in a blocking task since verification is CPU-intensive.
    let result = tokio::task::spawn_blocking(move || {
        temper_verify::VerificationCascade::from_ioa(&ioa_source)
            .with_sim_seeds(5)
            .with_prop_test_cases(100)
            .run()
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(result))
}

/// GET /observe/simulation/{entity}?seed=N&ticks=M -- run deterministic simulation.
///
/// Runs a single-seed simulation with light fault injection and returns the result.
async fn run_simulation(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
    Query(params): Query<SimQueryParams>,
) -> Result<Json<temper_verify::SimulationResult>, StatusCode> {
    let ioa_source = {
        let registry = state.registry.read().unwrap();
        let mut found = None;
        for tenant_id in registry.tenant_ids() {
            if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
                found = Some(entity_spec.ioa_source.clone());
                break;
            }
        }
        found
    };

    let Some(ioa_source) = ioa_source else {
        return Err(StatusCode::NOT_FOUND);
    };

    let seed = params.seed.unwrap_or(42);
    let ticks = params.ticks.unwrap_or(200);

    let result = tokio::task::spawn_blocking(move || {
        let config = temper_verify::SimConfig {
            seed,
            max_ticks: ticks,
            num_actors: 3,
            max_actions_per_actor: 20,
            max_counter: 2,
            faults: temper_runtime::scheduler::FaultConfig::light(),
        };
        temper_verify::run_simulation_from_ioa(&ioa_source, &config)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(result))
}

/// GET /observe/entities/{entity_type}/{entity_id}/history -- entity event history.
///
/// Returns the full event log for an entity. Checks two sources in order:
/// 1. In-memory actor state (if the actor is currently loaded).
/// 2. Postgres event store (if configured, for inactive entities).
async fn get_entity_history(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((entity_type, entity_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let tenant = extract_tenant(&headers, &state);

    // Path 1: If the actor is loaded, read events from in-memory state.
    let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
    let actor_ref = {
        let registry = state.actor_registry.read().unwrap_or_else(|e| e.into_inner());
        registry.get(&actor_key).cloned()
    };

    if let Some(actor_ref) = actor_ref {
        if let Ok(response) = actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(5))
            .await
        {
            return Json(format_history_response(
                &entity_type,
                &entity_id,
                &response.state.events,
            ));
        }
    }

    // Path 2: Query Postgres event store directly (for inactive entities).
    if let Some(ref store) = state.event_store {
        let persistence_id = format!("{entity_type}:{entity_id}");
        if let Ok(envelopes) = store.read_events(&persistence_id, 0).await {
            let events: Vec<serde_json::Value> = envelopes
                .iter()
                .filter_map(|env| {
                    serde_json::from_value::<EntityEvent>(env.payload.clone()).ok()
                })
                .enumerate()
                .map(|(i, event)| {
                    serde_json::json!({
                        "sequence": i + 1,
                        "action": event.action,
                        "from_state": event.from_status,
                        "to_state": event.to_status,
                        "timestamp": event.timestamp,
                        "params": event.params,
                    })
                })
                .collect();

            return Json(serde_json::json!({
                "entity_type": entity_type,
                "entity_id": entity_id,
                "events": events,
            }));
        }
    }

    // No data sources available.
    Json(serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": [],
    }))
}

/// Format entity events into the history API response shape.
fn format_history_response(
    entity_type: &str,
    entity_id: &str,
    events: &[EntityEvent],
) -> serde_json::Value {
    let formatted: Vec<serde_json::Value> = events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            serde_json::json!({
                "sequence": i + 1,
                "action": e.action,
                "from_state": e.from_status,
                "to_state": e.to_status,
                "timestamp": e.timestamp,
                "params": e.params,
            })
        })
        .collect();

    serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": formatted,
    })
}

// ---------------------------------------------------------------------------
// Phase 2: SSE event stream
// ---------------------------------------------------------------------------

/// GET /observe/events/stream -- Server-Sent Events stream of entity transitions.
///
/// Subscribes to the broadcast channel and streams every `EntityStateChange`
/// as a JSON SSE event. Supports optional `?entity_type=X&entity_id=Y` filters.
async fn handle_event_stream(
    State(state): State<ServerState>,
    Query(params): Query<EventStreamParams>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let filter_type = params.entity_type;
    let filter_id = params.entity_id;

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(change) => {
                // Apply filters.
                if let Some(ref ft) = filter_type {
                    if change.entity_type != *ft {
                        return None;
                    }
                }
                if let Some(ref fi) = filter_id {
                    if change.entity_id != *fi {
                        return None;
                    }
                }
                let data = serde_json::to_string(&change).unwrap_or_default();
                Some(Ok(Event::default().event("state_change").data(data)))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Phase 6: Health & Metrics
// ---------------------------------------------------------------------------

/// GET /observe/health -- server health summary.
async fn handle_health(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let now = sim_now();
    let uptime = (now - state.start_time).num_seconds().max(0) as u64;

    let specs_loaded = {
        let registry = state.registry.read().unwrap_or_else(|e| e.into_inner());
        let mut count: u64 = 0;
        for tid in registry.tenant_ids() {
            count += registry.entity_types(tid).len() as u64;
        }
        count
    };

    let active_entities = {
        let reg = state.actor_registry.read().unwrap_or_else(|e| e.into_inner());
        reg.len() as u64
    };

    let transitions_total = state.metrics.transitions_total.load(Ordering::Relaxed);
    let errors_total = state.metrics.errors_total.load(Ordering::Relaxed);

    let event_store_type = if state.event_store.is_some() {
        "postgres"
    } else {
        "none"
    };

    Json(serde_json::json!({
        "status": "healthy",
        "uptime_seconds": uptime,
        "specs_loaded": specs_loaded,
        "active_entities": active_entities,
        "transitions_total": transitions_total,
        "errors_total": errors_total,
        "event_store": event_store_type,
    }))
}

/// GET /observe/metrics -- Prometheus text-format metrics.
async fn handle_metrics(State(state): State<ServerState>) -> (StatusCode, [(String, String); 1], String) {
    let mut lines = Vec::new();

    // -- temper_transitions_total --
    lines.push("# HELP temper_transitions_total Total entity state transitions.".to_string());
    lines.push("# TYPE temper_transitions_total counter".to_string());
    if let Ok(map) = state.metrics.transitions.read() {
        for (key, count) in map.iter() {
            // key format: "entity_type:action:true|false"
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            if parts.len() == 3 {
                lines.push(format!(
                    "temper_transitions_total{{entity_type=\"{}\",action=\"{}\",success=\"{}\"}} {}",
                    parts[0], parts[1], parts[2], count
                ));
            }
        }
    }

    // -- temper_guard_rejections_total (subset: success=false) --
    lines.push("# HELP temper_guard_rejections_total Total failed transitions (guard not met or unknown action).".to_string());
    lines.push("# TYPE temper_guard_rejections_total counter".to_string());
    if let Ok(map) = state.metrics.transitions.read() {
        for (key, count) in map.iter() {
            if key.ends_with(":false") {
                let parts: Vec<&str> = key.splitn(3, ':').collect();
                if parts.len() == 3 {
                    lines.push(format!(
                        "temper_guard_rejections_total{{entity_type=\"{}\",action=\"{}\"}} {}",
                        parts[0], parts[1], count
                    ));
                }
            }
        }
    }

    // -- temper_active_entities --
    lines.push("# HELP temper_active_entities Number of currently active entity actors.".to_string());
    lines.push("# TYPE temper_active_entities gauge".to_string());
    {
        // Count per entity_type from the entity index.
        let index = state.entity_index.read().unwrap_or_else(|e| e.into_inner());
        for (key, ids) in index.iter() {
            // key format: "tenant:entity_type"
            if let Some(entity_type) = key.split(':').nth(1) {
                lines.push(format!(
                    "temper_active_entities{{entity_type=\"{}\"}} {}",
                    entity_type,
                    ids.len()
                ));
            }
        }
    }

    lines.push(String::new()); // trailing newline
    let body = lines.join("\n");

    (
        StatusCode::OK,
        [("Content-Type".to_string(), "text/plain; version=0.0.4; charset=utf-8".to_string())],
        body,
    )
}

// ---------------------------------------------------------------------------
// Phase 3: Trajectory tracking & failed intent capture
// ---------------------------------------------------------------------------

/// Query parameters for the trajectory aggregation endpoint.
#[derive(Deserialize)]
pub struct TrajectoryQueryParams {
    /// Filter by entity type.
    pub entity_type: Option<String>,
    /// Filter by action name.
    pub action: Option<String>,
    /// Filter by success/failure ("true" or "false").
    pub success: Option<String>,
    /// Maximum number of failed intents to return in the response (default: 50).
    pub failed_limit: Option<usize>,
}

/// GET /observe/trajectories -- aggregated trajectory stats with failed intent details.
///
/// Returns:
/// - `total`: total matching entries
/// - `success_count` / `error_count` / `success_rate`
/// - `by_action`: per-action breakdown
/// - `failed_intents`: most recent failed entries (up to `failed_limit`)
async fn handle_trajectories(
    State(state): State<ServerState>,
    Query(params): Query<TrajectoryQueryParams>,
) -> Json<serde_json::Value> {
    let failed_limit = params.failed_limit.unwrap_or(50).min(500);
    let success_filter: Option<bool> = params.success.as_deref().map(|s| s == "true");

    let log = state.trajectory_log.read().unwrap_or_else(|e| e.into_inner());

    // Filter entries.
    let filtered: Vec<&TrajectoryEntry> = log
        .entries()
        .iter()
        .filter(|e| {
            if let Some(ref ft) = params.entity_type {
                if e.entity_type != *ft {
                    return false;
                }
            }
            if let Some(ref fa) = params.action {
                if e.action != *fa {
                    return false;
                }
            }
            if let Some(sf) = success_filter {
                if e.success != sf {
                    return false;
                }
            }
            true
        })
        .collect();

    let total = filtered.len() as u64;
    let success_count = filtered.iter().filter(|e| e.success).count() as u64;
    let error_count = total - success_count;
    let success_rate = if total > 0 {
        success_count as f64 / total as f64
    } else {
        0.0
    };

    // Per-action breakdown (BTreeMap for deterministic order).
    let mut by_action: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    for entry in &filtered {
        let stats = by_action
            .entry(entry.action.clone())
            .or_insert_with(|| serde_json::json!({"total": 0u64, "success": 0u64, "error": 0u64}));
        if let Some(obj) = stats.as_object_mut() {
            *obj.entry("total").or_insert(serde_json::json!(0)) =
                serde_json::json!(obj["total"].as_u64().unwrap_or(0) + 1);
            if entry.success {
                *obj.entry("success").or_insert(serde_json::json!(0)) =
                    serde_json::json!(obj["success"].as_u64().unwrap_or(0) + 1);
            } else {
                *obj.entry("error").or_insert(serde_json::json!(0)) =
                    serde_json::json!(obj["error"].as_u64().unwrap_or(0) + 1);
            }
        }
    }

    // Collect most recent failed intents.
    let failed_intents: Vec<serde_json::Value> = filtered
        .iter()
        .rev()
        .filter(|e| !e.success)
        .take(failed_limit)
        .map(|e| {
            serde_json::json!({
                "timestamp": e.timestamp,
                "tenant": e.tenant,
                "entity_type": e.entity_type,
                "entity_id": e.entity_id,
                "action": e.action,
                "from_status": e.from_status,
                "error": e.error,
            })
        })
        .collect();

    Json(serde_json::json!({
        "total": total,
        "success_count": success_count,
        "error_count": error_count,
        "success_rate": success_rate,
        "by_action": by_action,
        "failed_intents": failed_intents,
    }))
}

// ---------------------------------------------------------------------------
// Phase 4: Sentinel Anomaly Detection
// ---------------------------------------------------------------------------

/// POST /observe/sentinel/check -- trigger sentinel rule evaluation.
///
/// Evaluates all default sentinel rules against current server state.
/// Any triggered rules generate O-Records and store them in the RecordStore.
/// Returns a list of alerts (may be empty if all is healthy).
async fn handle_sentinel_check(
    State(state): State<ServerState>,
) -> Json<serde_json::Value> {
    let rules = sentinel::default_rules();
    let alerts = sentinel::check_rules(&rules, &state);

    // Store generated O-Records.
    let mut results = Vec::new();
    for alert in &alerts {
        state.record_store.insert_observation(alert.record.clone());
        results.push(serde_json::json!({
            "rule": alert.rule_name,
            "record_id": alert.record.header.id,
            "source": alert.record.source,
            "classification": alert.record.classification,
            "threshold": alert.record.threshold_value,
            "observed": alert.record.observed_value,
        }));
    }

    Json(serde_json::json!({
        "alerts_count": alerts.len(),
        "alerts": results,
    }))
}

// ---------------------------------------------------------------------------
// Phase 5: Evolution Engine API
// ---------------------------------------------------------------------------

/// Query parameters for listing evolution records.
#[derive(Deserialize)]
pub struct EvolutionRecordParams {
    /// Filter by record type: "observation", "problem", "analysis", "decision", "insight".
    pub record_type: Option<String>,
    /// Filter by status: "open", "resolved", "superseded", "rejected".
    pub status: Option<String>,
}

/// GET /observe/evolution/records -- list all evolution records.
async fn list_evolution_records(
    State(state): State<ServerState>,
    Query(params): Query<EvolutionRecordParams>,
) -> Json<serde_json::Value> {
    let store = &state.record_store;
    let mut records: Vec<serde_json::Value> = Vec::new();

    let type_filter = params.record_type.as_deref();
    let status_filter = params.status.as_deref().and_then(parse_record_status);

    // Collect from each record type (only those matching the filter).
    if type_filter.is_none() || type_filter == Some("observation") {
        for obs in store.open_observations() {
            if matches_status_filter(&obs.header.status, &status_filter) {
                records.push(serde_json::json!({
                    "id": obs.header.id,
                    "record_type": "Observation",
                    "status": format!("{:?}", obs.header.status),
                    "created_by": obs.header.created_by,
                    "timestamp": obs.header.timestamp.to_rfc3339(),
                    "source": obs.source,
                    "classification": obs.classification,
                }));
            }
        }
    }

    if type_filter.is_none() || type_filter == Some("insight") {
        for insight in store.ranked_insights() {
            if matches_status_filter(&insight.header.status, &status_filter) {
                records.push(serde_json::json!({
                    "id": insight.header.id,
                    "record_type": "Insight",
                    "status": format!("{:?}", insight.header.status),
                    "created_by": insight.header.created_by,
                    "timestamp": insight.header.timestamp.to_rfc3339(),
                    "category": insight.category,
                    "priority_score": insight.priority_score,
                    "recommendation": insight.recommendation,
                }));
            }
        }
    }

    // For types not exposed via aggregation methods, use count as summary.
    if type_filter.is_none() || type_filter == Some("problem") {
        let count = store.count(RecordType::Problem);
        if count > 0 {
            records.push(serde_json::json!({
                "record_type": "Problem",
                "count": count,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }
    }
    if type_filter.is_none() || type_filter == Some("analysis") {
        let count = store.count(RecordType::Analysis);
        if count > 0 {
            records.push(serde_json::json!({
                "record_type": "Analysis",
                "count": count,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }
    }
    if type_filter.is_none() || type_filter == Some("decision") {
        let count = store.count(RecordType::Decision);
        if count > 0 {
            records.push(serde_json::json!({
                "record_type": "Decision",
                "count": count,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }
    }

    Json(serde_json::json!({
        "records": records,
        "total_observations": store.count(RecordType::Observation),
        "total_problems": store.count(RecordType::Problem),
        "total_analyses": store.count(RecordType::Analysis),
        "total_decisions": store.count(RecordType::Decision),
        "total_insights": store.count(RecordType::Insight),
    }))
}

/// GET /observe/evolution/records/{id} -- get a single record with chain info.
async fn get_evolution_record(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let store = &state.record_store;

    // Try each record type.
    if let Some(obs) = store.get_observation(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": obs,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }
    if let Some(problem) = store.get_problem(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": problem,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }
    if let Some(analysis) = store.get_analysis(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": analysis,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }
    if let Some(decision) = store.get_decision(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": decision,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }
    if let Some(insight) = store.get_insight(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": insight,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }

    Err(StatusCode::NOT_FOUND)
}

/// Request body for the decide endpoint.
#[derive(Deserialize)]
pub struct DecideRequest {
    /// The decision: "approved", "rejected", or "deferred".
    pub decision: String,
    /// Who is making the decision (email or identifier).
    pub decided_by: String,
    /// Human rationale for the decision.
    pub rationale: String,
}

/// POST /observe/evolution/records/{id}/decide -- create a D-Record for a record.
///
/// The target record (by ID) must exist. Creates a DecisionRecord derived from it.
async fn handle_decide(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(body): Json<DecideRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let store = &state.record_store;

    // Verify the target record exists.
    let exists = store.get_observation(&id).is_some()
        || store.get_problem(&id).is_some()
        || store.get_analysis(&id).is_some()
        || store.get_decision(&id).is_some()
        || store.get_insight(&id).is_some();

    if !exists {
        return Err(StatusCode::NOT_FOUND);
    }

    let decision = match body.decision.to_lowercase().as_str() {
        "approved" | "approve" => Decision::Approved,
        "rejected" | "reject" => Decision::Rejected,
        "deferred" | "defer" => Decision::Deferred,
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    // Build the D-Record with DST-safe timestamps.
    let now = sim_now();
    let id_suffix = &temper_runtime::scheduler::sim_uuid().to_string()[..8];
    let year = now.format("%Y");
    let record_id = format!("D-{year}-{id_suffix}");

    let d_record = DecisionRecord {
        header: RecordHeader {
            id: record_id.clone(),
            record_type: RecordType::Decision,
            timestamp: now,
            created_by: body.decided_by.clone(),
            derived_from: Some(id.clone()),
            status: RecordStatus::Open,
        },
        decision,
        decided_by: body.decided_by,
        rationale: body.rationale,
        verification_results: None,
        implementation: None,
    };

    store.insert_decision(d_record.clone());

    Ok(Json(serde_json::json!({
        "record_id": record_id,
        "decision": format!("{:?}", d_record.decision),
        "derived_from": id,
        "status": "Open",
    })))
}

/// GET /observe/evolution/insights -- list ranked insights (I-Records).
async fn list_evolution_insights(
    State(state): State<ServerState>,
) -> Json<serde_json::Value> {
    let insights = state.record_store.ranked_insights();

    let items: Vec<serde_json::Value> = insights
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.header.id,
                "category": i.category,
                "priority_score": i.priority_score,
                "recommendation": i.recommendation,
                "signal": i.signal,
                "status": format!("{:?}", i.header.status),
                "timestamp": i.header.timestamp.to_rfc3339(),
            })
        })
        .collect();

    Json(serde_json::json!({
        "insights": items,
        "total": items.len(),
    }))
}

/// Parse a status filter string into a RecordStatus.
fn parse_record_status(s: &str) -> Option<RecordStatus> {
    match s.to_lowercase().as_str() {
        "open" => Some(RecordStatus::Open),
        "resolved" => Some(RecordStatus::Resolved),
        "superseded" => Some(RecordStatus::Superseded),
        "rejected" => Some(RecordStatus::Rejected),
        _ => None,
    }
}

/// Check if a record's status matches the optional filter.
fn matches_status_filter(status: &RecordStatus, filter: &Option<RecordStatus>) -> bool {
    match filter {
        Some(f) => status == f,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use temper_runtime::ActorSystem;
    use temper_runtime::tenant::TenantId;
    use temper_spec::csdl::parse_csdl;
    use tower::ServiceExt;

    use crate::registry::SpecRegistry;

    const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    fn test_state_with_registry() -> ServerState {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA)],
        );
        let system = ActorSystem::new("test-observe");
        ServerState::from_registry(system, registry)
    }

    fn build_test_app() -> Router {
        let state = test_state_with_registry();
        Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state)
    }

    #[tokio::test]
    async fn test_list_specs_returns_registered_entities() {
        let app = build_test_app();
        let response = app
            .oneshot(Request::get("/observe/specs").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let specs: Vec<SpecSummary> = serde_json::from_slice(&body).unwrap();
        assert!(!specs.is_empty());
        assert_eq!(specs[0].entity_type, "Order");
        assert!(!specs[0].states.is_empty());
        assert!(!specs[0].actions.is_empty());
    }

    #[tokio::test]
    async fn test_get_spec_detail_found() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/specs/Order")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let detail: SpecDetail = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail.entity_type, "Order");
        assert!(!detail.states.is_empty());
        assert!(!detail.actions.is_empty());
    }

    #[tokio::test]
    async fn test_get_spec_detail_not_found() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/specs/NonExistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_entities_empty() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/entities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let entities: Vec<EntityInstanceSummary> = serde_json::from_slice(&body).unwrap();
        // No actors spawned yet, so should be empty
        assert!(entities.is_empty());
    }

    #[tokio::test]
    async fn test_entity_history_returns_events() {
        let state = test_state_with_registry();

        // Dispatch actions to build an event log.
        let r = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "order-hist-1",
                "AddItem",
                serde_json::json!({"ProductId": "p1"}),
            )
            .await;
        assert!(r.is_ok(), "AddItem failed: {r:?}");

        let r = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "order-hist-1",
                "SubmitOrder",
                serde_json::json!({}),
            )
            .await;
        assert!(r.is_ok(), "SubmitOrder failed: {r:?}");

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::get("/observe/entities/Order/order-hist-1/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["entity_type"], "Order");
        assert_eq!(json["entity_id"], "order-hist-1");

        let events = json["events"].as_array().expect("events should be array");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["action"], "AddItem");
        assert_eq!(events[0]["from_state"], "Draft");
        assert_eq!(events[0]["to_state"], "Draft");
        assert_eq!(events[1]["action"], "SubmitOrder");
        assert_eq!(events[1]["to_state"], "Submitted");
    }

    #[tokio::test]
    async fn test_entity_history_empty_for_unknown() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/entities/Order/nonexistent/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["entity_type"], "Order");
        assert_eq!(json["entity_id"], "nonexistent");
        let events = json["events"].as_array().expect("events should be array");
        assert!(events.is_empty());
    }

    // -- Health endpoint tests --

    #[tokio::test]
    async fn test_health_returns_status() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "healthy");
        assert!(json["specs_loaded"].as_u64().is_some());
        assert_eq!(json["event_store"], "none");
    }

    #[tokio::test]
    async fn test_health_counts_entities_and_transitions() {
        let state = test_state_with_registry();

        // Dispatch an action to create an entity and increment metrics.
        let r = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "health-test-1",
                "AddItem",
                serde_json::json!({}),
            )
            .await;
        assert!(r.is_ok());

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::get("/observe/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["active_entities"], 1);
        assert_eq!(json["transitions_total"], 1);
        assert_eq!(json["errors_total"], 0);
    }

    // -- Metrics endpoint tests --

    #[tokio::test]
    async fn test_metrics_returns_prometheus_format() {
        let state = test_state_with_registry();

        // Dispatch a successful and a failed action to populate metrics.
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "metrics-1",
                "AddItem",
                serde_json::json!({}),
            )
            .await;
        // SubmitOrder with 0 items should fail.
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "metrics-2",
                "SubmitOrder",
                serde_json::json!({}),
            )
            .await;

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::get("/observe/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("text/plain"), "content-type should be text/plain, got: {ct}");

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("temper_transitions_total"),
            "should contain transitions metric"
        );
        assert!(
            text.contains("temper_active_entities"),
            "should contain active entities metric"
        );
    }

    // -- Trajectory endpoint tests --

    #[tokio::test]
    async fn test_trajectories_records_success_and_failure() {
        let state = test_state_with_registry();

        // Successful action.
        let r = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "traj-1",
                "AddItem",
                serde_json::json!({"ProductId": "p1"}),
            )
            .await;
        assert!(r.is_ok());

        // Failed action (SubmitOrder on a brand-new entity with no items guard).
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "traj-2",
                "SubmitOrder",
                serde_json::json!({}),
            )
            .await;

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::get("/observe/trajectories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json["total"].as_u64().unwrap() >= 2);
        assert!(json["success_count"].as_u64().unwrap() >= 1);
        assert!(json["error_count"].as_u64().unwrap() >= 1);
        assert!(json["success_rate"].as_f64().unwrap() > 0.0);
        assert!(json["success_rate"].as_f64().unwrap() < 1.0);

        // by_action should have keys for dispatched actions.
        let by_action = json["by_action"].as_object().unwrap();
        assert!(by_action.contains_key("AddItem"));

        // failed_intents should contain at least one entry.
        let failed = json["failed_intents"].as_array().unwrap();
        assert!(!failed.is_empty());
        assert!(failed[0]["error"].is_string());
    }

    #[tokio::test]
    async fn test_trajectories_filters_by_entity_type() {
        let state = test_state_with_registry();

        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "traj-f1",
                "AddItem",
                serde_json::json!({"ProductId": "p1"}),
            )
            .await;

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        // Filter for entity_type=Order should find our entry.
        let response = app.clone()
            .oneshot(
                Request::get("/observe/trajectories?entity_type=Order")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["total"].as_u64().unwrap() >= 1);

        // Filter for non-existent entity_type should return 0.
        let response = app
            .oneshot(
                Request::get("/observe/trajectories?entity_type=Nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], 0);
    }

    #[tokio::test]
    async fn test_trajectories_empty_when_no_actions() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::get("/observe/trajectories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], 0);
        assert_eq!(json["success_count"], 0);
        assert_eq!(json["error_count"], 0);
        assert_eq!(json["success_rate"], 0.0);
        let failed = json["failed_intents"].as_array().unwrap();
        assert!(failed.is_empty());
    }

    // -- Sentinel endpoint tests --

    #[tokio::test]
    async fn test_sentinel_check_no_alerts_on_clean_state() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::post("/observe/sentinel/check")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["alerts_count"], 0);
        let alerts = json["alerts"].as_array().unwrap();
        assert!(alerts.is_empty());
    }

    #[tokio::test]
    async fn test_sentinel_check_detects_error_spike() {
        let state = test_state_with_registry();

        // Generate high error rate (>10%).
        for i in 0..8 {
            let _ = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Order",
                    &format!("sentinel-fail-{i}"),
                    "SubmitOrder",
                    serde_json::json!({}),
                )
                .await;
        }
        for i in 0..2 {
            let _ = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Order",
                    &format!("sentinel-pass-{i}"),
                    "AddItem",
                    serde_json::json!({"ProductId": "p1"}),
                )
                .await;
        }

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::post("/observe/sentinel/check")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["alerts_count"].as_u64().unwrap() >= 1);

        let alerts = json["alerts"].as_array().unwrap();
        let error_alert = alerts.iter().find(|a| a["rule"] == "error_rate_spike");
        assert!(error_alert.is_some(), "should detect error rate spike");

        let alert = error_alert.unwrap();
        assert!(alert["record_id"].as_str().unwrap().starts_with("O-"));
    }

    // -- Evolution API endpoint tests --

    #[tokio::test]
    async fn test_evolution_records_empty() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::get("/observe/evolution/records")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_observations"], 0);
        assert_eq!(json["total_decisions"], 0);
    }

    #[tokio::test]
    async fn test_evolution_records_after_sentinel() {
        let state = test_state_with_registry();

        // Generate errors to trigger sentinel.
        for i in 0..10 {
            let _ = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Order",
                    &format!("evo-fail-{i}"),
                    "SubmitOrder",
                    serde_json::json!({}),
                )
                .await;
        }

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        // Trigger sentinel first.
        let _ = app.clone()
            .oneshot(
                Request::post("/observe/sentinel/check")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Now check evolution records.
        let response = app
            .oneshot(
                Request::get("/observe/evolution/records")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["total_observations"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn test_evolution_get_record_not_found() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::get("/observe/evolution/records/O-2024-nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_evolution_decide_creates_d_record() {
        let state = test_state_with_registry();

        // Manually insert an O-Record.
        let obs = temper_evolution::ObservationRecord {
            header: temper_evolution::RecordHeader {
                id: "O-test-decide".to_string(),
                record_type: temper_evolution::RecordType::Observation,
                timestamp: sim_now(),
                created_by: "test".to_string(),
                derived_from: None,
                status: temper_evolution::RecordStatus::Open,
            },
            source: "test".to_string(),
            classification: temper_evolution::ObservationClass::ErrorRate,
            evidence_query: "test query".to_string(),
            threshold_field: None,
            threshold_value: None,
            observed_value: None,
            context: serde_json::json!({}),
        };
        state.record_store.insert_observation(obs);

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        // Create a D-Record decision.
        let response = app.clone()
            .oneshot(
                Request::post("/observe/evolution/records/O-test-decide/decide")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"decision":"approved","decided_by":"alice@example.com","rationale":"Looks good"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["record_id"].as_str().unwrap().starts_with("D-"));
        assert_eq!(json["decision"], "Approved");
        assert_eq!(json["derived_from"], "O-test-decide");
    }

    #[tokio::test]
    async fn test_evolution_decide_not_found() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::post("/observe/evolution/records/O-nonexistent/decide")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"decision":"rejected","decided_by":"bob","rationale":"nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_evolution_insights_empty() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::get("/observe/evolution/insights")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], 0);
        let insights = json["insights"].as_array().unwrap();
        assert!(insights.is_empty());
    }
}
