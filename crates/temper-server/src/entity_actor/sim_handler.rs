//! Simulation handler for entity actors.
//!
//! [`EntityActorHandler`] wraps a real [`TransitionTable`] and [`EntityState`].
//! Its [`SimActorHandler`] path shares production evaluation, effects, and event
//! recording without async persistence or telemetry.

use std::sync::Arc;

use temper_jit::table::{EvalContext, TransitionTable};
use temper_runtime::scheduler::{CompareOp, SimActorHandler, SpecAssert, SpecInvariant};
use temper_spec::automaton::StateVar;

use super::effects::ScheduledAction;
use super::types::EntityState;

mod replay;

/// Simulation handler wrapping a real TransitionTable.
///
/// This is the bridge that lets [`SimActorSystem`] exercise the identical
/// `TransitionTable::evaluate()` path used in production, with deterministic
/// clock and ID generation.
#[derive(Clone)]
pub struct EntityActorHandler {
    table: Arc<TransitionTable>,
    state: EntityState,
    invariants: Vec<SpecInvariant>,
    /// Custom effects from the last successful action (integration triggers).
    last_custom_effects: Vec<String>,
    /// Scheduled actions from the last successful action (timer requests).
    last_scheduled_actions: Vec<ScheduledAction>,
    /// Deterministic durable journal used to reconstruct volatile state.
    journal: Vec<super::types::EntityEvent>,
    /// Counter declarations whose absent map entry denotes the spec's zero initial value.
    declared_counters: std::collections::BTreeSet<String>,
    /// Boolean declarations whose absent map entry denotes the spec's false initial value.
    declared_bools: std::collections::BTreeSet<String>,
}

impl EntityActorHandler {
    /// Stable fingerprint of simulation-visible derived state, excluding journal history.
    pub fn state_fingerprint(&self) -> String {
        serde_json::to_string(&(
            &self.state.status,
            self.state.item_count,
            &self.state.counters,
            &self.state.booleans,
            &self.state.lists,
            &self.state.fields,
        ))
        .expect("entity simulation state must serialize")
    }
    fn fresh_state(entity_type: String, entity_id: String, table: &TransitionTable) -> EntityState {
        let mut fields = serde_json::json!({});
        super::effects::canonicalize_entity_fields(&mut fields, &entity_id, &table.initial_state);

        EntityState {
            entity_type,
            entity_id,
            status: table.initial_state.clone(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields,
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        }
    }

    /// Create a new simulation handler for an entity.
    pub fn new(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        table: Arc<TransitionTable>,
    ) -> Self {
        let entity_type = entity_type.into();
        let entity_id = entity_id.into();
        let state = Self::fresh_state(entity_type, entity_id, &table);

        Self {
            table,
            state,
            invariants: Vec::new(),
            last_custom_effects: Vec::new(),
            last_scheduled_actions: Vec::new(),
            journal: Vec::new(),
            declared_counters: std::collections::BTreeSet::new(),
            declared_bools: std::collections::BTreeSet::new(),
        }
    }

    fn record_committed_event(&mut self, event: super::types::EntityEvent) {
        assert!(
            self.journal.len() < super::types::MAX_EVENTS_SINCE_SNAPSHOT,
            "simulation journal budget exhausted"
        );
        let sequence_nr = self.state.sequence_nr.saturating_add(1);
        self.journal.push(event.clone());
        self.state.record_committed_event(event, sequence_nr);
        assert_eq!(self.journal.len(), self.state.total_event_count);
    }

    /// Build an [`EvalContext`] from the current entity state.
    fn eval_context(&self) -> EvalContext {
        super::effects::build_eval_context(&self.state)
    }

    /// Attach spec invariants parsed from I/O Automaton TOML source.
    ///
    /// The [`SimActorSystem`] checks these automatically after every
    /// successful transition — no manual `set_invariant_checker()` needed.
    pub fn with_ioa_invariants(mut self, ioa_toml: &str) -> Self {
        let automaton = temper_spec::automaton::parse_automaton(ioa_toml)
            .expect("failed to parse I/O Automaton TOML for invariants");
        let declared_bools: std::collections::BTreeSet<_> = automaton
            .state
            .iter()
            .filter(|state| is_declared_bool(state))
            .map(|state| state.name.clone())
            .collect();
        self.declared_counters = automaton
            .state
            .iter()
            .filter(|state| state.var_type == "counter")
            .map(|state| state.name.clone())
            .collect();
        self.declared_bools = declared_bools.clone();

        self.invariants = automaton
            .invariants
            .iter()
            .map(|inv| {
                let assert_kind =
                    parse_assert_expr(&inv.assert, &declared_bools).unwrap_or_else(|| {
                        panic!(
                            "invariant '{}' expression {:?} is not simulation-checkable",
                            inv.name, inv.assert
                        )
                    });
                SpecInvariant {
                    name: inv.name.clone(),
                    when: inv.when.clone(),
                    assert: assert_kind,
                }
            })
            .collect();

        self
    }

    /// Apply the same exact-sequence field mutation used by the live actor.
    pub fn update_fields(
        &mut self,
        fields: serde_json::Value,
        replace: bool,
        expected_sequence: Option<u64>,
    ) -> bool {
        self.update_fields_with_reference_evidence(
            fields,
            replace,
            expected_sequence,
            &std::collections::BTreeMap::new(),
        )
    }

    /// Apply a simulated field write with deterministic target-existence evidence.
    pub fn update_fields_with_reference_evidence(
        &mut self,
        fields: serde_json::Value,
        replace: bool,
        expected_sequence: Option<u64>,
        reference_evidence: &std::collections::BTreeMap<String, bool>,
    ) -> bool {
        if expected_sequence.is_some_and(|expected| expected != self.state.sequence_nr) {
            return false;
        }
        if !self.state.can_accept_event() {
            return false;
        }
        let event = super::types::EntityEvent {
            action: super::types::FIELD_UPDATE_EVENT_TYPE.into(),
            from_status: self.state.status.clone(),
            to_status: self.state.status.clone(),
            timestamp: temper_runtime::scheduler::sim_now(),
            params: serde_json::json!({"replace": replace, "fields": fields}),
            idempotency_key: None,
        };
        let event_fields = event
            .params
            .get("fields")
            .expect("field-update event always contains fields");
        let mut prospective = self.state.clone();
        assert!(
            super::effects::apply_field_update(&mut prospective, event_fields, replace),
            "field-update event and entity fields must be objects"
        );
        if super::reference_contract::validate_prospective_state(
            &self.table,
            super::types::FIELD_UPDATE_EVENT_TYPE,
            &self.state,
            &prospective,
            reference_evidence,
        )
        .is_err()
        {
            return false;
        }
        self.state = prospective;
        self.record_committed_event(event);
        true
    }

    /// Execute the production action path with deterministic reference evidence.
    pub fn handle_action_with_reference_evidence(
        &mut self,
        action: &str,
        params: serde_json::Value,
        reference_evidence: &std::collections::BTreeMap<String, bool>,
    ) -> bool {
        let result = super::effects::process_action_with_xref(
            &mut self.state,
            &self.table,
            action,
            &params,
            reference_evidence,
        );
        if let Some(event) = result.event {
            self.record_committed_event(event);
        }
        result.success
    }
}

/// Map a shared [`ParsedAssert`] to the runtime [`SpecAssert`].
///
/// Uses [`temper_spec::automaton::parse_assert_expr`] as the single parser,
/// then maps the result to the runtime type. Returns `None` for expressions
/// that the framework cannot check automatically.
fn parse_assert_expr(
    expr: &str,
    declared_bools: &std::collections::BTreeSet<String>,
) -> Option<SpecAssert> {
    if let Some(var) = expr.trim().strip_prefix("is_true ") {
        return declared_bools
            .contains(var)
            .then(|| SpecAssert::BoolRequired {
                var: var.to_string(),
                expect: true,
            });
    }
    if let Some(var) = expr
        .trim()
        .strip_suffix(" != ''")
        .or_else(|| expr.trim().strip_suffix(" != \"\""))
        .or_else(|| expr.trim().strip_suffix(" !="))
    {
        return Some(SpecAssert::StringNonEmpty {
            var: var.trim().to_string(),
        });
    }
    let parts: Vec<_> = expr.split_whitespace().collect();
    if let [left, op, right] = parts.as_slice()
        && right.parse::<usize>().is_err()
    {
        let op = match *op {
            ">" => CompareOp::Gt,
            ">=" => CompareOp::Gte,
            "<" => CompareOp::Lt,
            "<=" => CompareOp::Lte,
            "==" => CompareOp::Eq,
            _ => return None,
        };
        return Some(SpecAssert::CounterCompareCounter {
            left: (*left).to_string(),
            op,
            right: (*right).to_string(),
        });
    }
    use temper_spec::automaton::parse_assert_expr as parse;
    translate_parsed(parse(expr)?, declared_bools)
}

fn translate_parsed(
    parsed: temper_spec::automaton::ParsedAssert,
    declared_bools: &std::collections::BTreeSet<String>,
) -> Option<SpecAssert> {
    use temper_spec::automaton::{AssertCompareOp, ParsedAssert};

    match parsed {
        ParsedAssert::CounterPositive { var } => Some(SpecAssert::CounterPositive { var }),
        ParsedAssert::NoFurtherTransitions => Some(SpecAssert::NoFurtherTransitions),
        ParsedAssert::OrderingConstraint { before, after } => {
            Some(SpecAssert::OrderingConstraint { before, after })
        }
        ParsedAssert::NeverState { state } => Some(SpecAssert::NeverState { state }),
        ParsedAssert::CounterCompare { var, op, value } => {
            let runtime_op = match op {
                AssertCompareOp::Gt => CompareOp::Gt,
                AssertCompareOp::Gte => CompareOp::Gte,
                AssertCompareOp::Lt => CompareOp::Lt,
                AssertCompareOp::Lte => CompareOp::Lte,
                AssertCompareOp::Eq => CompareOp::Eq,
            };
            Some(SpecAssert::CounterCompare {
                var,
                op: runtime_op,
                value,
            })
        }
        ParsedAssert::BoolRequired { var, expect } => declared_bools
            .contains(&var)
            .then_some(SpecAssert::BoolRequired { var, expect }),
        ParsedAssert::And(parts) => {
            let mapped: Option<Vec<_>> = parts
                .into_iter()
                .map(|part| translate_parsed(part, declared_bools))
                .collect();
            mapped.map(SpecAssert::And)
        }
        ParsedAssert::Or(parts) => {
            let mapped: Option<Vec<_>> = parts
                .into_iter()
                .map(|part| translate_parsed(part, declared_bools))
                .collect();
            mapped.map(SpecAssert::Or)
        }
    }
}

fn is_declared_bool(state: &StateVar) -> bool {
    state.var_type == "bool"
}

impl SimActorHandler for EntityActorHandler {
    fn init(&mut self) -> Result<serde_json::Value, String> {
        self.state = Self::fresh_state(
            self.state.entity_type.clone(),
            self.state.entity_id.clone(),
            &self.table,
        );
        self.journal.clear();

        Ok(serde_json::to_value(&self.state).unwrap_or_default())
    }

    fn restart(&mut self) -> Result<serde_json::Value, String> {
        let journal = self.journal.clone();
        let total_event_count = journal.len();
        self.state = Self::fresh_state(
            self.state.entity_type.clone(),
            self.state.entity_id.clone(),
            &self.table,
        );
        replay::rebuild(&mut self.state, &self.table, journal)?;
        self.last_custom_effects.clear();
        self.last_scheduled_actions.clear();
        assert_eq!(self.state.total_event_count, total_event_count);
        Ok(serde_json::to_value(&self.state).unwrap_or_default())
    }

    fn handle_message(&mut self, action: &str, params: &str) -> Result<serde_json::Value, String> {
        let params_value: serde_json::Value =
            serde_json::from_str(params).unwrap_or(serde_json::json!({}));

        // Unified process_action — THE SAME CODE as production.
        // FoundationDB DST principle: one function for all paths.
        let result =
            super::effects::process_action(&mut self.state, &self.table, action, &params_value);

        if result.success {
            // Capture custom effects for integration callback scheduling
            self.last_custom_effects = result.custom_effects;
            self.last_scheduled_actions = result.scheduled_actions;
            if let Some(event) = result.event {
                self.record_committed_event(event);
            }
            Ok(serde_json::to_value(&self.state).unwrap_or_default())
        } else {
            self.last_custom_effects.clear();
            self.last_scheduled_actions.clear();
            Err(result.error.unwrap_or_else(|| "Unknown error".to_string()))
        }
    }

    fn current_status(&self) -> String {
        self.state.status.clone()
    }

    fn current_item_count(&self) -> usize {
        self.state.item_count
    }

    fn event_count(&self) -> usize {
        self.state.total_event_count
    }

    fn event_sequence(&self) -> u64 {
        self.state.sequence_nr
    }

    fn valid_actions(&self) -> Vec<String> {
        let ctx = self.eval_context();
        self.table
            .rules
            .iter()
            .filter(|rule| {
                let state_ok = rule.from_states.is_empty()
                    || rule.from_states.iter().any(|s| s == &self.state.status);
                if !state_ok {
                    return false;
                }
                rule.guard.check(&self.state.status, &ctx)
            })
            .map(|rule| rule.name.clone())
            .collect()
    }

    fn events_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.state.events).unwrap_or(serde_json::Value::Array(vec![]))
    }

    fn spec_invariants(&self) -> &[SpecInvariant] {
        &self.invariants
    }

    fn bool_field(&self, var: &str) -> Option<bool> {
        self.state
            .booleans
            .get(var)
            .copied()
            .or_else(|| self.declared_bools.contains(var).then_some(false))
    }

    fn counter_value(&self, var: &str) -> Option<usize> {
        if var == "items" {
            Some(self.state.item_count)
        } else {
            self.state
                .counters
                .get(var)
                .copied()
                .or_else(|| self.declared_counters.contains(var).then_some(0))
        }
    }

    fn string_value(&self, var: &str) -> Option<String> {
        self.state
            .fields
            .get(var)
            .or_else(|| self.state.fields.get(to_pascal_case(var)))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    fn pending_callbacks(&self) -> Vec<String> {
        self.last_custom_effects.clone()
    }
}

fn to_pascal_case(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut uppercase = true;
    for character in name.chars() {
        if character == '_' {
            uppercase = true;
        } else if uppercase {
            result.extend(character.to_uppercase());
            uppercase = false;
        } else {
            result.push(character);
        }
    }
    result
}

#[cfg(test)]
mod tests;
