//! Health and metrics endpoints.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::RwLock;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use temper_runtime::scheduler::sim_now;

use crate::state::ServerState;

/// Format a colon-delimited key map into Prometheus exposition lines.
///
/// Each key is split into exactly 3 parts by `:`. Keys with fewer parts are
/// skipped. The three parts are assigned to `labels[0..3]` respectively.
fn format_keyed_metric(
    lines: &mut Vec<String>,
    map: &RwLock<BTreeMap<String, u64>>,
    metric_name: &str,
    labels: [&str; 3],
) {
    if let Ok(m) = map.read() {
        for (key, count) in m.iter() {
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            if parts.len() == 3 {
                lines.push(format!(
                    "{metric_name}{{{l0}=\"{v0}\",{l1}=\"{v1}\",{l2}=\"{v2}\"}} {count}",
                    l0 = labels[0],
                    v0 = parts[0],
                    l1 = labels[1],
                    v1 = parts[1],
                    l2 = labels[2],
                    v2 = parts[2],
                ));
            }
        }
    }
}

/// GET /observe/health -- server health summary.
pub(crate) async fn handle_health(State(state): State<ServerState>) -> Json<serde_json::Value> {
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
        let reg = state
            .actor_registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        reg.len() as u64
    };

    let transitions_total = state.metrics.transitions_total.load(Ordering::Relaxed);
    let errors_total = state.metrics.errors_total.load(Ordering::Relaxed);

    let event_store_type = state
        .event_store
        .as_ref()
        .map(|store| store.backend_name())
        .unwrap_or("none");

    Json(serde_json::json!({
        "status": "healthy",
        "uptime_seconds": uptime,
        "specs_loaded": specs_loaded,
        "active_entities": active_entities,
        "transitions_total": transitions_total,
        "errors_total": errors_total,
        "event_store": event_store_type,
        "cross_invariant_enforce": state.cross_invariant_enforce,
        "cross_invariant_eventual_enforce": state.cross_invariant_eventual_enforce,
    }))
}

/// GET /observe/metrics -- Prometheus text-format metrics.
pub(crate) async fn handle_metrics(
    State(state): State<ServerState>,
) -> (StatusCode, [(String, String); 1], String) {
    let mut lines = Vec::new();

    // -- temper_transitions_total --
    lines.push("# HELP temper_transitions_total Total entity state transitions.".to_string());
    lines.push("# TYPE temper_transitions_total counter".to_string());
    format_keyed_metric(
        &mut lines,
        &state.metrics.transitions,
        "temper_transitions_total",
        ["entity_type", "action", "success"],
    );

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
    lines.push(
        "# HELP temper_active_entities Number of currently active entity actors.".to_string(),
    );
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

    // -- temper_cross_invariant_checks_total --
    lines.push(
        "# HELP temper_cross_invariant_checks_total Total cross-invariant checks.".to_string(),
    );
    lines.push("# TYPE temper_cross_invariant_checks_total counter".to_string());
    format_keyed_metric(
        &mut lines,
        &state.metrics.cross_invariant_checks,
        "temper_cross_invariant_checks_total",
        ["tenant", "entity_type", "result"],
    );

    // -- temper_cross_invariant_violations_total --
    lines.push(
        "# HELP temper_cross_invariant_violations_total Total cross-invariant violations."
            .to_string(),
    );
    lines.push("# TYPE temper_cross_invariant_violations_total counter".to_string());
    format_keyed_metric(
        &mut lines,
        &state.metrics.cross_invariant_violations,
        "temper_cross_invariant_violations_total",
        ["tenant", "invariant", "kind"],
    );

    // -- temper_relation_integrity_violations_total --
    lines.push(
        "# HELP temper_relation_integrity_violations_total Total relation integrity violations."
            .to_string(),
    );
    lines.push("# TYPE temper_relation_integrity_violations_total counter".to_string());
    format_keyed_metric(
        &mut lines,
        &state.metrics.relation_integrity_violations,
        "temper_relation_integrity_violations_total",
        ["tenant", "entity_type", "operation"],
    );

    // -- temper_cross_invariant_eval_duration_ms_bucket --
    lines.push("# HELP temper_cross_invariant_eval_duration_ms_bucket Cross-invariant evaluation latency histogram buckets.".to_string());
    lines.push("# TYPE temper_cross_invariant_eval_duration_ms_bucket histogram".to_string());
    if let Ok(map) = state.metrics.cross_invariant_eval_duration_ms_bucket.read() {
        for (bucket, count) in map.iter() {
            lines.push(format!(
                "temper_cross_invariant_eval_duration_ms_bucket{{le=\"{}\"}} {}",
                bucket, count
            ));
        }
    }

    // -- temper_cross_invariant_bypass_total --
    lines.push(
        "# HELP temper_cross_invariant_bypass_total Total cross-invariant bypass uses.".to_string(),
    );
    lines.push("# TYPE temper_cross_invariant_bypass_total counter".to_string());
    lines.push(format!(
        "temper_cross_invariant_bypass_total {}",
        state
            .metrics
            .cross_invariant_bypass_total
            .load(std::sync::atomic::Ordering::Relaxed)
    ));

    lines.push(String::new()); // trailing newline
    let body = lines.join("\n");

    (
        StatusCode::OK,
        [(
            "Content-Type".to_string(),
            "text/plain; version=0.0.4; charset=utf-8".to_string(),
        )],
        body,
    )
}
