//! GEPA Pareto WASM module.
//!
//! Maintains GEPA-style frontier support mappings:
//! - frontier key -> candidates supporting that local frontier
//! - dominated-support reduction
//! - deterministic candidate selection by support frequency

use std::collections::{BTreeMap, BTreeSet};

use temper_wasm_sdk::prelude::*;

type FrontierMapping = BTreeMap<String, BTreeSet<String>>;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-pareto: updating frontier support mappings");

        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);

        let candidate_payload =
            read_candidate_payload(&ctx, fields).ok_or("unable to read candidate payload")?;
        let candidate_id = candidate_payload
            .get("id")
            .and_then(Value::as_str)
            .ok_or("candidate missing 'id'")?
            .to_string();
        if candidate_payload
            .get("scores")
            .and_then(Value::as_object)
            .is_none()
        {
            return Err("candidate missing 'scores'".into());
        }

        let mut all_candidates = read_previous_candidates(fields);
        all_candidates.insert(candidate_id.clone(), candidate_payload.clone());

        let aggregate_scores = build_aggregate_scores(&all_candidates);
        let frontier_mapping = build_frontier_mapping(&all_candidates);
        let reduced_mapping = remove_dominated_programs(&frontier_mapping, &aggregate_scores);
        let new_dominators = flatten_mapping_ids(&reduced_mapping);

        let previous_dominators = read_previous_dominators(fields);
        let added = new_dominators.contains(&candidate_id) && !previous_dominators.contains(&candidate_id);

        let removed: Vec<String> = previous_dominators
            .difference(&new_dominators)
            .cloned()
            .collect();

        let selected_candidate_id = select_candidate_from_frontier(&reduced_mapping, &aggregate_scores);

        let reduced_frontier_candidates: Vec<Value> = new_dominators
            .iter()
            .filter_map(|id| all_candidates.get(id).cloned())
            .collect();

        let frontier_mapping_json = mapping_to_json(&reduced_mapping);
        let frontier_update = json!({
            "added": added,
            "removed": removed,
            "dominators": new_dominators.iter().cloned().collect::<Vec<_>>(),
            "selected_candidate_id": selected_candidate_id,
            "frontier_size": reduced_frontier_candidates.len(),
            "frontier_mapping": frontier_mapping_json,
            "pareto_frontier": reduced_frontier_candidates,
        });

        ctx.log(
            "info",
            &format!(
                "gepa-pareto: candidate={}, added={}, frontier_size={}, selected={}",
                candidate_id,
                added,
                frontier_update.get("frontier_size").and_then(Value::as_u64).unwrap_or(0),
                frontier_update
                    .get("selected_candidate_id")
                    .and_then(Value::as_str)
                    .unwrap_or("none")
            ),
        );

        Ok(json!({
            "FrontierUpdateJson": frontier_update.to_string(),
            "frontier_update": frontier_update,
            "pareto_frontier": frontier_update["pareto_frontier"].clone(),
            "frontier_mapping": frontier_update["frontier_mapping"].clone(),
            "selected_candidate_id": frontier_update["selected_candidate_id"].clone(),
            "added": added,
            "removed": frontier_update["removed"].clone(),
        }))
    }
}

fn read_candidate_payload(ctx: &Context, fields: &Value) -> Option<Value> {
    if let Some(candidate) = ctx.trigger_params.get("candidate") {
        return Some(candidate.clone());
    }

    if let Some(scores_json) = ctx.trigger_params.get("ScoresJson") {
        return Some(parse_or_clone(scores_json));
    }

    if let Some(scores) = ctx.trigger_params.get("scores").and_then(Value::as_object) {
        let candidate_id = fields
            .get("CandidateId")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("CandidateId").and_then(Value::as_str))
            .unwrap_or("candidate-unknown");
        return Some(json!({
            "id": candidate_id,
            "scores": Value::Object(scores.clone()),
        }));
    }

    None
}

fn parse_or_clone(value: &Value) -> Value {
    match value {
        Value::String(raw) => serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!({})),
        _ => value.clone(),
    }
}

fn read_previous_candidates(fields: &Value) -> BTreeMap<String, Value> {
    let mut candidates = BTreeMap::<String, Value>::new();

    // Prefer explicit previous frontier payload.
    if let Some(frontier) = fields.get("pareto_frontier").and_then(Value::as_array) {
        for candidate in frontier {
            if let Some(id) = candidate.get("id").and_then(Value::as_str) {
                candidates.insert(id.to_string(), candidate.clone());
            }
        }
    }

    // Fallback: parse last FrontierUpdateJson if present.
    if candidates.is_empty() {
        if let Some(frontier_update_json) = fields.get("FrontierUpdateJson") {
            let parsed = parse_or_clone(frontier_update_json);
            if let Some(frontier) = parsed.get("pareto_frontier").and_then(Value::as_array) {
                for candidate in frontier {
                    if let Some(id) = candidate.get("id").and_then(Value::as_str) {
                        candidates.insert(id.to_string(), candidate.clone());
                    }
                }
            }
        }
    }

    candidates
}

fn read_previous_dominators(fields: &Value) -> BTreeSet<String> {
    if let Some(frontier_update_json) = fields.get("FrontierUpdateJson") {
        let parsed = parse_or_clone(frontier_update_json);
        if let Some(ids) = parsed.get("dominators").and_then(Value::as_array) {
            return ids
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
    }

    fields
        .get("pareto_frontier")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn build_aggregate_scores(candidates: &BTreeMap<String, Value>) -> BTreeMap<String, f64> {
    let mut scores = BTreeMap::new();
    for (id, candidate) in candidates {
        let aggregate = candidate
            .get("scores")
            .and_then(Value::as_object)
            .map(|obj| {
                if let Some(weighted_sum) = obj.get("weighted_sum").and_then(Value::as_f64) {
                    weighted_sum
                } else {
                    let mut total = 0.0;
                    let mut count = 0.0;
                    for v in obj.values() {
                        if let Some(n) = v.as_f64() {
                            total += n;
                            count += 1.0;
                        }
                    }
                    if count > 0.0 { total / count } else { 0.0 }
                }
            })
            .unwrap_or(0.0);
        scores.insert(id.clone(), aggregate);
    }
    scores
}

fn build_frontier_mapping(candidates: &BTreeMap<String, Value>) -> FrontierMapping {
    let mut objective_max = BTreeMap::<String, f64>::new();
    for candidate in candidates.values() {
        if let Some(scores) = candidate.get("scores").and_then(Value::as_object) {
            for (objective, score) in scores {
                let val = score.as_f64().unwrap_or(0.0);
                let current = objective_max.get(objective).copied().unwrap_or(f64::NEG_INFINITY);
                if val > current {
                    objective_max.insert(objective.clone(), val);
                }
            }
        }
    }

    let mut mapping = FrontierMapping::new();
    for (id, candidate) in candidates {
        if let Some(scores) = candidate.get("scores").and_then(Value::as_object) {
            for (objective, score) in scores {
                let val = score.as_f64().unwrap_or(0.0);
                let max_val = objective_max
                    .get(objective)
                    .copied()
                    .unwrap_or(f64::NEG_INFINITY);
                if (val - max_val).abs() <= 1e-12 {
                    mapping.entry(objective.clone()).or_default().insert(id.clone());
                }
            }
        }
    }
    mapping
}

fn flatten_mapping_ids(mapping: &FrontierMapping) -> BTreeSet<String> {
    mapping
        .values()
        .flat_map(|front| front.iter().cloned())
        .collect()
}

fn remove_dominated_programs(
    mapping: &FrontierMapping,
    aggregate_scores: &BTreeMap<String, f64>,
) -> FrontierMapping {
    let mut freq = BTreeMap::<String, usize>::new();
    for front in mapping.values() {
        for candidate_id in front {
            *freq.entry(candidate_id.clone()).or_insert(0) += 1;
        }
    }

    let mut programs: Vec<String> = freq.keys().cloned().collect();
    programs.sort_by(|a, b| {
        let a_score = aggregate_scores.get(a).copied().unwrap_or(0.0);
        let b_score = aggregate_scores.get(b).copied().unwrap_or(0.0);
        a_score
            .partial_cmp(&b_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b))
    });

    let mut dominated = BTreeSet::<String>::new();
    let mut changed = true;
    while changed {
        changed = false;
        for y in &programs {
            if dominated.contains(y) {
                continue;
            }

            let others: BTreeSet<String> = programs
                .iter()
                .filter(|p| *p != y && !dominated.contains(*p))
                .cloned()
                .collect();

            if is_dominated_in_mapping(y, &others, mapping) {
                dominated.insert(y.clone());
                changed = true;
                break;
            }
        }
    }

    let dominators: BTreeSet<String> = programs
        .into_iter()
        .filter(|p| !dominated.contains(p))
        .collect();

    let mut reduced = FrontierMapping::new();
    for (key, front) in mapping {
        let filtered: BTreeSet<String> = front
            .iter()
            .filter(|candidate_id| dominators.contains(*candidate_id))
            .cloned()
            .collect();
        if !filtered.is_empty() {
            reduced.insert(key.clone(), filtered);
        }
    }
    reduced
}

fn is_dominated_in_mapping(
    candidate_id: &str,
    other_candidates: &BTreeSet<String>,
    mapping: &FrontierMapping,
) -> bool {
    let fronts_for_candidate: Vec<&BTreeSet<String>> = mapping
        .values()
        .filter(|front| front.contains(candidate_id))
        .collect();
    if fronts_for_candidate.is_empty() {
        return false;
    }

    for front in fronts_for_candidate {
        let found_dominator = front.iter().any(|other| other_candidates.contains(other));
        if !found_dominator {
            return false;
        }
    }
    true
}

fn select_candidate_from_frontier(
    mapping: &FrontierMapping,
    aggregate_scores: &BTreeMap<String, f64>,
) -> Value {
    let mut frequency = BTreeMap::<String, usize>::new();
    for front in mapping.values() {
        for candidate_id in front {
            *frequency.entry(candidate_id.clone()).or_insert(0) += 1;
        }
    }

    let selected = frequency.into_iter().max_by(|(id_a, freq_a), (id_b, freq_b)| {
        freq_a
            .cmp(freq_b)
            .then_with(|| {
                let score_a = aggregate_scores.get(id_a).copied().unwrap_or(0.0);
                let score_b = aggregate_scores.get(id_b).copied().unwrap_or(0.0);
                score_a
                    .partial_cmp(&score_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| id_b.cmp(id_a))
    });

    match selected {
        Some((id, _)) => json!(id),
        None => Value::Null,
    }
}

fn mapping_to_json(mapping: &FrontierMapping) -> Value {
    let mut obj = serde_json::Map::<String, Value>::new();
    for (key, ids) in mapping {
        obj.insert(
            key.clone(),
            Value::Array(ids.iter().cloned().map(Value::String).collect()),
        );
    }
    Value::Object(obj)
}
