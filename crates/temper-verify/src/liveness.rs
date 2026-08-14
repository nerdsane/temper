//! Leads-to checks that honor `from` on `[[liveness]] reaches`.
//!
//! Stateright `Property::eventually` is evaluated from the initial state
//! and ignores `from`. A property `from = ReadingQuery, reaches = Idle`
//! is then true as soon as the machine starts in Idle — even when
//! `ReadingQuery` is a trap. This module checks the intended reading:
//! every reachable state whose status is in `from` can still reach a
//! status in `targets`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use stateright::Model;

use crate::model::{LivenessKind, TemperModel, TemperModelState};

/// Names of `ReachesState` properties that fail on `model`.
pub fn unreachable_leads_to(model: &TemperModel) -> Vec<String> {
    let (ids, statuses, adj) = explore_entity(model);
    let mut failed = Vec::new();
    for live in &model.liveness {
        let LivenessKind::ReachesState { from, targets } = &live.kind else {
            continue;
        };
        if targets.is_empty() {
            continue;
        }
        for (id, status) in ids.iter().zip(statuses.iter()) {
            if !from.is_empty() && !from.iter().any(|s| s == status) {
                continue;
            }
            if targets.iter().any(|s| s == status) {
                continue;
            }
            if !can_reach_status(id, targets, &ids, &statuses, &adj) {
                failed.push(live.name.clone());
                break;
            }
        }
    }
    failed
}

/// Names of per-entity `ReachesState` properties that fail on a joint graph.
///
/// `status_by_entity[key][entity] = status`. Keys match `adj`.
pub fn unreachable_leads_to_joint(
    models: &BTreeMap<String, TemperModel>,
    status_by_entity: &BTreeMap<String, BTreeMap<String, String>>,
    adj: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<String> {
    let ids: Vec<String> = status_by_entity.keys().cloned().collect();
    let mut failed = Vec::new();
    for (entity, model) in models {
        for live in &model.liveness {
            let LivenessKind::ReachesState { from, targets } = &live.kind else {
                continue;
            };
            if targets.is_empty() {
                continue;
            }
            let statuses: Vec<String> = ids
                .iter()
                .map(|id| {
                    status_by_entity[id]
                        .get(entity)
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            for (id, status) in ids.iter().zip(statuses.iter()) {
                if !from.is_empty() && !from.iter().any(|s| s == status) {
                    continue;
                }
                if targets.iter().any(|s| s == status) {
                    continue;
                }
                if !can_reach_status(id, targets, &ids, &statuses, adj) {
                    failed.push(format!("{entity}.{}", live.name));
                    break;
                }
            }
        }
    }
    failed
}

fn explore_entity(
    model: &TemperModel,
) -> (Vec<String>, Vec<String>, BTreeMap<String, BTreeSet<String>>) {
    let mut ids = Vec::new();
    let mut statuses = Vec::new();
    let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    for init in model.init_states() {
        let id = entity_id(&init);
        if seen.insert(id.clone()) {
            queue.push_back(init);
        }
    }
    while let Some(state) = queue.pop_front() {
        let from = entity_id(&state);
        let mut actions = Vec::new();
        model.actions(&state, &mut actions);
        for action in actions {
            let Some(next) = model.next_state(&state, action) else {
                continue;
            };
            let to = entity_id(&next);
            adj.entry(from.clone()).or_default().insert(to.clone());
            if seen.insert(to) {
                queue.push_back(next);
            }
        }
        ids.push(from);
        statuses.push(state.status);
    }
    (ids, statuses, adj)
}

fn can_reach_status(
    start: &str,
    targets: &[String],
    ids: &[String],
    statuses: &[String],
    adj: &BTreeMap<String, BTreeSet<String>>,
) -> bool {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(start.to_string());
    seen.insert(start.to_string());
    while let Some(key) = queue.pop_front() {
        if let Some(idx) = ids.iter().position(|id| id == &key)
            && targets.iter().any(|t| t == &statuses[idx])
        {
            return true;
        }
        if let Some(nexts) = adj.get(&key) {
            for n in nexts {
                if seen.insert(n.clone()) {
                    queue.push_back(n.clone());
                }
            }
        }
    }
    false
}

fn entity_id(state: &TemperModelState) -> String {
    state.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::automaton::parse_automaton;

    #[test]
    fn trap_in_from_fails_leads_to() {
        let spec = r#"
[automaton]
name = "CuratorAgent"
states = ["Idle", "ReadingQuery"]
initial = "Idle"
allow_indefinite_states = ["Idle", "ReadingQuery"]

[[action]]
name = "TakeQuery"
from = ["Idle"]
to = "ReadingQuery"

[[liveness]]
name = "QueryEventuallyResolves"
from = ["ReadingQuery"]
reaches = ["Idle"]
"#;
        let aut = parse_automaton(spec).unwrap();
        let model = crate::model::build_model_from_automaton(&aut, 3);
        assert_eq!(
            unreachable_leads_to(&model),
            vec!["QueryEventuallyResolves".to_string()]
        );
    }

    #[test]
    fn path_back_to_target_passes() {
        let spec = r#"
[automaton]
name = "CuratorAgent"
states = ["Idle", "ReadingQuery"]
initial = "Idle"
allow_indefinite_states = ["Idle"]

[[action]]
name = "TakeQuery"
from = ["Idle"]
to = "ReadingQuery"

[[action]]
name = "Abandon"
from = ["ReadingQuery"]
to = "Idle"

[[liveness]]
name = "QueryEventuallyResolves"
from = ["ReadingQuery"]
reaches = ["Idle"]
"#;
        let aut = parse_automaton(spec).unwrap();
        let model = crate::model::build_model_from_automaton(&aut, 3);
        assert!(unreachable_leads_to(&model).is_empty());
    }
}
