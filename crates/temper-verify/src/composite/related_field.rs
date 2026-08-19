//! Hard related-field sidecar rules as a joint enabled-action property (ADR-0171).
//!
//! A hard row from `cross-invariants.toml` fails when a matching action is
//! **enabled** in a reachable joint state while `related(...).field` does
//! not hold. The sidecar is never synthesized as extra `actions()` guards.

use std::collections::BTreeMap;

use temper_spec::cross_invariant::{
    CrossInvariantOperator, CrossInvariantSpec, InvariantKind, parse_related_field_assert,
    split_trigger,
};

use stateright::Model;

use crate::model::TemperModelState;

use super::model::{CompositeState, CompositeTemperModel, state_key};

/// Compiled hard related-field constraint for the joint checker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedFieldRule {
    /// Sidecar row name (e.g. `PublishNeedsThisReviewRecorded`).
    pub name: String,
    /// Entity named by `on` (`HumanCurator` in `HumanCurator.Publish`).
    pub on_entity: String,
    /// Action named by `on`, or `None` for `Entity.*`.
    pub on_action: Option<String>,
    /// `related()` target entity type.
    pub target_entity: String,
    /// FK field on the `on` entity (display / one-instance resolution).
    pub source_field: String,
    /// Field read on the target slice (`status` is the v1 that must work).
    pub field_name: String,
    /// `in` / `not in`.
    pub operator: CrossInvariantOperator,
    /// Literal values from the sidecar assert.
    pub values: Vec<String>,
}

/// Why a hard related-field rule failed in a joint state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelatedFieldFailReason {
    /// Target field was readable and the `in` / `not in` check failed.
    AssertFailed {
        /// Target entity type.
        target_entity: String,
        /// Field that was read.
        field: String,
        /// Value read from the target slice.
        actual: String,
    },
    /// Named field is not `status` and not a bool/counter on `TemperModelState`.
    UnreadableField {
        /// Target entity type.
        target_entity: String,
        /// Field that could not be read.
        field: String,
    },
    /// Target type was not in the joint state (should be rare after composition).
    TargetNotInScope {
        /// Missing target entity type.
        target_entity: String,
    },
}

/// Named violation of a hard related-field rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedFieldViolation {
    /// Sidecar row name.
    pub name: String,
    /// Entity whose enabled action matched `on`.
    pub on_entity: String,
    /// Enabled action that matched (or `*` when reporting a wildcard miss).
    pub on_action: String,
    /// Why the rule failed.
    pub reason: RelatedFieldFailReason,
}

impl std::fmt::Display for RelatedFieldViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "related-field constraint '{}' violated: {}.{} is enabled",
            self.name, self.on_entity, self.on_action
        )?;
        match &self.reason {
            RelatedFieldFailReason::AssertFailed {
                target_entity,
                field,
                actual,
            } => write!(
                f,
                " while related({target_entity}, …).{field} is '{actual}'"
            ),
            RelatedFieldFailReason::UnreadableField {
                target_entity,
                field,
            } => write!(
                f,
                " but related({target_entity}, …).{field} is not readable on TemperModelState"
            ),
            RelatedFieldFailReason::TargetNotInScope { target_entity } => {
                write!(
                    f,
                    " but target entity '{target_entity}' is not in the joint state"
                )
            }
        }
    }
}

/// Hard rows compiled for the joint property. Eventual rows are omitted.
pub fn compile_hard_related_field_rules(spec: &CrossInvariantSpec) -> Vec<RelatedFieldRule> {
    spec.invariants
        .iter()
        .filter(|inv| inv.kind == InvariantKind::Hard)
        .filter_map(|inv| {
            let (on_entity, on_action) = split_trigger(&inv.on)?;
            let assertion = parse_related_field_assert(&inv.assertion)?;
            Some(RelatedFieldRule {
                name: inv.name.clone(),
                on_entity: on_entity.to_string(),
                on_action: on_action.map(str::to_string),
                target_entity: assertion.target_entity,
                source_field: assertion.source_field,
                field_name: assertion.field_name,
                operator: assertion.operator,
                values: assertion.values,
            })
        })
        .collect()
}

/// Undirected `on` ↔ `related()` pairs from hard rows, for seed cover and
/// plan reachability.
pub fn related_composition_pairs(spec: &CrossInvariantSpec) -> Vec<(String, String, String)> {
    compile_hard_related_field_rules(spec)
        .into_iter()
        .map(|rule| (rule.on_entity, rule.target_entity, rule.name))
        .collect()
}

/// Names of eventual-kind rows (runtime only; verify warns, does not gate).
pub fn eventual_constraint_names(spec: &CrossInvariantSpec) -> Vec<String> {
    spec.invariants
        .iter()
        .filter(|inv| inv.kind == InvariantKind::Eventual)
        .map(|inv| inv.name.clone())
        .collect()
}

/// Whether `entity.action` matches this rule's `on` selector.
pub fn action_matches_rule(rule: &RelatedFieldRule, entity: &str, action: &str) -> bool {
    if rule.on_entity != entity {
        return false;
    }
    match &rule.on_action {
        None => true,
        Some(name) => name == action,
    }
}

/// Read a sidecar field from one entity slice. `status` is the v1 path.
/// Bools stringify as `true`/`false`; counters as decimal. Anything else
/// is unreadable (caller fail-closes).
pub fn read_model_field(state: &TemperModelState, field: &str) -> Option<String> {
    if field == "status" {
        return Some(state.status.clone());
    }
    if let Some(flag) = state.booleans.get(field) {
        return Some(if *flag {
            "true".to_string()
        } else {
            "false".to_string()
        });
    }
    if let Some(count) = state.counters.get(field) {
        return Some(count.to_string());
    }
    None
}

fn assert_holds(rule: &RelatedFieldRule, actual: &str) -> bool {
    match rule.operator {
        CrossInvariantOperator::In => rule.values.iter().any(|v| v == actual),
        CrossInvariantOperator::NotIn => rule.values.iter().all(|v| v != actual),
    }
}

/// Violations of `rules` given the joint model's currently enabled actions.
///
/// `enabled` is `(entity, action)` pairs from [`super::model::CompositeTemperModel`]'s
/// `actions()` — the sidecar must not have been used to filter that set.
pub fn violations_in_state(
    rules: &[RelatedFieldRule],
    enabled: &[(String, String)],
    entities: &BTreeMap<String, TemperModelState>,
) -> Vec<RelatedFieldViolation> {
    let mut out = Vec::new();
    for rule in rules {
        let matching: Vec<&(String, String)> = enabled
            .iter()
            .filter(|(entity, action)| action_matches_rule(rule, entity, action))
            .collect();
        if matching.is_empty() {
            continue;
        }
        for (on_entity, on_action) in matching {
            out.push(evaluate_enabled_rule(rule, on_entity, on_action, entities));
        }
    }
    out.into_iter().flatten().collect()
}

fn evaluate_enabled_rule(
    rule: &RelatedFieldRule,
    on_entity: &str,
    on_action: &str,
    entities: &BTreeMap<String, TemperModelState>,
) -> Option<RelatedFieldViolation> {
    let Some(target) = entities.get(&rule.target_entity) else {
        return Some(RelatedFieldViolation {
            name: rule.name.clone(),
            on_entity: on_entity.to_string(),
            on_action: on_action.to_string(),
            reason: RelatedFieldFailReason::TargetNotInScope {
                target_entity: rule.target_entity.clone(),
            },
        });
    };
    let Some(actual) = read_model_field(target, &rule.field_name) else {
        return Some(RelatedFieldViolation {
            name: rule.name.clone(),
            on_entity: on_entity.to_string(),
            on_action: on_action.to_string(),
            reason: RelatedFieldFailReason::UnreadableField {
                target_entity: rule.target_entity.clone(),
                field: rule.field_name.clone(),
            },
        });
    };
    if assert_holds(rule, &actual) {
        return None;
    }
    Some(RelatedFieldViolation {
        name: rule.name.clone(),
        on_entity: on_entity.to_string(),
        on_action: on_action.to_string(),
        reason: RelatedFieldFailReason::AssertFailed {
            target_entity: rule.target_entity.clone(),
            field: rule.field_name.clone(),
            actual,
        },
    })
}

impl CompositeTemperModel {
    /// Enabled `(entity, action)` pairs — the same set [`Model::actions`]
    /// advertises. Used by the related-field property so the sidecar is not
    /// applied as a guard.
    pub(super) fn enabled_external_actions(&self, state: &CompositeState) -> Vec<(String, String)> {
        let mut actions = Vec::new();
        self.actions(state, &mut actions);
        actions
            .into_iter()
            .map(|a| (a.entity, a.action.name))
            .collect()
    }

    /// Distinct hard related-field violations reachable in the joint space.
    pub fn enumerate_related_field_violations(
        &self,
        state_budget: usize,
    ) -> Vec<RelatedFieldViolation> {
        use std::collections::BTreeSet;
        use std::collections::VecDeque;

        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut hits: Vec<RelatedFieldViolation> = Vec::new();
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<CompositeState> = VecDeque::new();

        for init in self.init_states() {
            if visited.insert(state_key(&init)) {
                queue.push_back(init);
            }
        }

        while let Some(state) = queue.pop_front() {
            if visited.len() >= state_budget {
                break;
            }
            for violation in violations_in_state(
                &self.related_field_rules,
                &self.enabled_external_actions(&state),
                &state.entities,
            ) {
                if seen.insert(violation.name.clone()) {
                    hits.push(violation);
                }
            }
            let mut actions = Vec::new();
            self.actions(&state, &mut actions);
            for action in actions {
                let Some(next) = self.next_state(&state, action) else {
                    continue;
                };
                if visited.insert(state_key(&next)) {
                    queue.push_back(next);
                }
            }
        }

        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::cross_invariant::parse_cross_invariants;

    fn sidecar() -> CrossInvariantSpec {
        parse_cross_invariants(
            r#"
[[invariant]]
name = "PublishNeedsThisReviewRecorded"
kind = "hard"
on = "HumanCurator.Publish"
assert = 'related(ReviewAgent, review_agent_id).status in ["VerdictRecorded"]'
"#,
        )
        .unwrap()
    }

    #[test]
    fn compile_skips_eventual() {
        let spec = parse_cross_invariants(
            r#"
[[invariant]]
name = "HardOne"
kind = "hard"
on = "A.Go"
assert = 'related(B, b_id).status in ["Ok"]'

[[invariant]]
name = "Later"
kind = "eventual"
on = "A.Go"
assert = 'related(B, b_id).status in ["Ok"]'
window_ms = 1000
"#,
        )
        .unwrap();
        let rules = compile_hard_related_field_rules(&spec);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "HardOne");
        assert_eq!(eventual_constraint_names(&spec), vec!["Later".to_string()]);
    }

    #[test]
    fn unguarded_publish_fails_when_review_is_reviewing() {
        let rules = compile_hard_related_field_rules(&sidecar());
        let enabled = vec![("HumanCurator".into(), "Publish".into())];
        let mut entities = BTreeMap::new();
        entities.insert(
            "HumanCurator".into(),
            TemperModelState {
                status: "Reviewing".into(),
                counters: BTreeMap::new(),
                booleans: BTreeMap::new(),
                lists: BTreeMap::new(),
            },
        );
        entities.insert(
            "ReviewAgent".into(),
            TemperModelState {
                status: "Reviewing".into(),
                counters: BTreeMap::new(),
                booleans: BTreeMap::new(),
                lists: BTreeMap::new(),
            },
        );
        let hits = violations_in_state(&rules, &enabled, &entities);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "PublishNeedsThisReviewRecorded");
    }

    #[test]
    fn holds_when_review_is_recorded() {
        let rules = compile_hard_related_field_rules(&sidecar());
        let enabled = vec![("HumanCurator".into(), "Publish".into())];
        let mut entities = BTreeMap::new();
        entities.insert(
            "HumanCurator".into(),
            TemperModelState {
                status: "Reviewing".into(),
                counters: BTreeMap::new(),
                booleans: BTreeMap::new(),
                lists: BTreeMap::new(),
            },
        );
        entities.insert(
            "ReviewAgent".into(),
            TemperModelState {
                status: "VerdictRecorded".into(),
                counters: BTreeMap::new(),
                booleans: BTreeMap::new(),
                lists: BTreeMap::new(),
            },
        );
        assert!(violations_in_state(&rules, &enabled, &entities).is_empty());
    }

    #[test]
    fn skipped_when_matching_action_not_enabled() {
        let rules = compile_hard_related_field_rules(&sidecar());
        let enabled = vec![("HumanCurator".into(), "RecordReviewVerdict".into())];
        let mut entities = BTreeMap::new();
        entities.insert(
            "ReviewAgent".into(),
            TemperModelState {
                status: "Reviewing".into(),
                counters: BTreeMap::new(),
                booleans: BTreeMap::new(),
                lists: BTreeMap::new(),
            },
        );
        assert!(violations_in_state(&rules, &enabled, &entities).is_empty());
    }

    #[test]
    fn unreadable_field_fails_closed() {
        let spec = parse_cross_invariants(
            r#"
[[invariant]]
name = "NeedsConfigType"
kind = "hard"
on = "Child.*"
assert = 'related(Parent, parent_id).ConfigType in ["Cloud"]'
"#,
        )
        .unwrap();
        let rules = compile_hard_related_field_rules(&spec);
        let enabled = vec![("Child".into(), "Go".into())];
        let mut entities = BTreeMap::new();
        entities.insert(
            "Parent".into(),
            TemperModelState {
                status: "Active".into(),
                counters: BTreeMap::new(),
                booleans: BTreeMap::new(),
                lists: BTreeMap::new(),
            },
        );
        let hits = violations_in_state(&rules, &enabled, &entities);
        assert_eq!(hits.len(), 1);
        assert!(matches!(
            hits[0].reason,
            RelatedFieldFailReason::UnreadableField { .. }
        ));
    }
}
