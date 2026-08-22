//! Generated direct-simulation coverage for IOA specifications.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use temper_jit::table::{Guard, TransitionTable};
use temper_runtime::scheduler::SimActorHandler;

use super::sim_handler::EntityActorHandler;

/// Bounded coverage observed while exploring one IOA specification.
#[derive(Debug, Clone, Default)]
pub struct SpecSimulationCoverage {
    /// Declared entity name.
    pub entity_type: String,
    /// States reached by production transition evaluation and effect application.
    pub states: BTreeSet<String>,
    /// Actions successfully executed by the simulation handler.
    pub actions: BTreeSet<String>,
    /// Number of generated successor scenarios executed.
    pub generated_scenarios: usize,
    /// Invariant names installed into the handler evaluator.
    pub invariants: BTreeSet<String>,
}

/// Explore an IOA spec through cloned real entity handlers under a strict budget.
///
/// This is breadth-first state-machine exploration, not a parser-only manifest:
/// every reported action passed through `process_action`, shared effect
/// application, committed-event recording, and handler reconstruction data.
pub fn explore_ioa_spec(ioa_source: &str, scenario_budget: usize) -> SpecSimulationCoverage {
    assert!(
        scenario_budget > 0,
        "coverage scenario budget must be positive"
    );
    let automaton = temper_spec::automaton::parse_automaton(ioa_source)
        .expect("coverage IOA source must parse");
    let table = Arc::new(TransitionTable::from_automaton(&automaton));
    let mut initial = EntityActorHandler::new(
        automaton.automaton.name.clone(),
        "coverage-entity",
        Arc::clone(&table),
    )
    .with_ioa_invariants(ioa_source);
    initial.init().expect("coverage actor must initialize");

    let invariants = initial
        .spec_invariants()
        .iter()
        .map(|invariant| invariant.name.clone())
        .collect();
    let mut coverage = SpecSimulationCoverage {
        entity_type: automaton.automaton.name.clone(),
        invariants,
        ..Default::default()
    };
    coverage.states.insert(initial.current_status());

    let mut queue = VecDeque::from([initial]);
    let mut visited = BTreeSet::new();
    while let Some(handler) = queue.pop_front() {
        if coverage.generated_scenarios >= scenario_budget {
            break;
        }
        if !visited.insert(handler.state_fingerprint()) {
            continue;
        }

        for rule in &table.rules {
            if coverage.generated_scenarios >= scenario_budget {
                break;
            }
            let state_matches = rule.from_states.is_empty()
                || rule
                    .from_states
                    .iter()
                    .any(|state| state == &handler.current_status());
            if !state_matches {
                continue;
            }

            let mut successor = handler.clone();
            let params = coverage_params(&table, &rule.name);
            let mut evidence = BTreeMap::new();
            collect_cross_entity_evidence(&rule.guard, &mut evidence);
            coverage.generated_scenarios += 1;
            if successor.handle_action_with_reference_evidence(&rule.name, params, &evidence) {
                coverage.actions.insert(rule.name.clone());
                coverage.states.insert(successor.current_status());
                queue.push_back(successor);
            }
        }
    }

    assert!(
        coverage.generated_scenarios <= scenario_budget,
        "coverage exploration exceeded its scenario budget"
    );
    coverage
}

fn coverage_params(table: &TransitionTable, action: &str) -> serde_json::Value {
    let mut values = serde_json::Map::new();
    if let Some(params) = table.action_params.get(action) {
        for (name, metadata) in params {
            let lower_type = metadata.param_type.to_ascii_lowercase();
            let lower_name = name.to_ascii_lowercase();
            let value = if lower_type.contains("int")
                || lower_type.contains("counter")
                || ["amount", "count", "quantity", "priority", "size_bytes"]
                    .iter()
                    .any(|needle| lower_name.contains(needle))
            {
                serde_json::json!(1)
            } else if lower_type.contains("bool") {
                serde_json::json!(true)
            } else {
                serde_json::json!("coverage")
            };
            values.insert(name.clone(), value);
        }
    }
    serde_json::Value::Object(values)
}

fn collect_cross_entity_evidence(guard: &Guard, evidence: &mut BTreeMap<String, bool>) {
    match guard {
        Guard::CrossEntityStateIn {
            entity_type,
            entity_id_source,
            ..
        } => {
            evidence.insert(format!("__xref:{entity_type}:{entity_id_source}"), true);
        }
        Guard::And(guards) => {
            for guard in guards {
                collect_cross_entity_evidence(guard, evidence);
            }
        }
        Guard::Always
        | Guard::StateIn(_)
        | Guard::ItemCountMin(_)
        | Guard::CounterMin { .. }
        | Guard::CounterMax { .. }
        | Guard::BoolTrue(_)
        | Guard::BoolFalse(_)
        | Guard::ListContains { .. }
        | Guard::ListLengthMin { .. }
        | Guard::ReferenceEquals { .. } => {}
    }
}
