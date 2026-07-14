//! Simulation handler for entity actors.
//!
//! [`EntityActorHandler`] wraps a real [`TransitionTable`] and [`EntityState`],
//! implementing [`SimActorHandler`] for deterministic simulation. The
//! `handle_message()` method is the synchronous subset of the production
//! `EntityActor::handle()`: same `evaluate()` call, same effect application,
//! same event recording. No async, no persistence, no telemetry.

use std::sync::Arc;

use temper_jit::table::{EvalContext, TransitionTable};
use temper_runtime::scheduler::{CompareOp, SimActorHandler, SpecAssert, SpecInvariant};
use temper_spec::automaton::StateVar;

use super::effects::ScheduledAction;
use super::types::EntityState;

/// Simulation handler wrapping a real TransitionTable.
///
/// This is the bridge that lets [`SimActorSystem`] exercise the identical
/// `TransitionTable::evaluate()` path used in production, with deterministic
/// clock and ID generation.
pub struct EntityActorHandler {
    table: Arc<TransitionTable>,
    state: EntityState,
    initial_fields: serde_json::Value,
    invariants: Vec<SpecInvariant>,
    /// Custom effects from the last successful action (integration triggers).
    last_custom_effects: Vec<String>,
    /// Scheduled actions from the last successful action (timer requests).
    last_scheduled_actions: Vec<ScheduledAction>,
}

impl EntityActorHandler {
    /// Create a new simulation handler for an entity.
    pub fn new(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        table: Arc<TransitionTable>,
    ) -> Self {
        let entity_type = entity_type.into();
        let entity_id = entity_id.into();

        let state = super::effects::build_initial_entity_state(
            &entity_type,
            &entity_id,
            &table,
            &serde_json::json!({}),
        )
        .expect("compiled initial state values must be well typed");

        Self {
            table,
            state,
            initial_fields: serde_json::json!({}),
            invariants: Vec::new(),
            last_custom_effects: Vec::new(),
            last_scheduled_actions: Vec::new(),
        }
    }

    /// Supply the same creation fields used to initialize a production actor.
    pub fn with_initial_fields(mut self, initial_fields: serde_json::Value) -> Self {
        self.initial_fields = initial_fields;
        self
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
        let compiled_runtime = temper_spec::automaton::compile_runtime_invariants(&automaton);

        self.invariants = automaton
            .invariants
            .iter()
            .map(|inv| {
                let mut assert_kind = parse_assert_expr(&inv.assert, &declared_bools)
                    .unwrap_or_else(|| SpecAssert::Unsupported {
                        expression: inv.assert.clone(),
                    });
                if matches!(assert_kind, SpecAssert::RuntimeEnforced { .. }) {
                    let attached = compiled_runtime
                        .iter()
                        .find(|runtime| runtime.name == inv.name)
                        .is_some_and(|runtime| self.table.runtime_invariants.contains(runtime));
                    if !attached {
                        assert_kind = SpecAssert::Unsupported {
                            expression: inv.assert.clone(),
                        };
                    }
                }
                SpecInvariant {
                    name: inv.name.clone(),
                    when: inv.when.clone(),
                    assert: assert_kind,
                }
            })
            .collect();

        self
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
    use temper_spec::automaton::parse_assert_expr as parse;
    translate_parsed(parse(expr)?, declared_bools)
}

fn translate_parsed(
    parsed: temper_spec::automaton::ParsedAssert,
    declared_bools: &std::collections::BTreeSet<String>,
) -> Option<SpecAssert> {
    use temper_spec::automaton::{AssertCompareOp, ParsedAssert};

    match parsed {
        ParsedAssert::Always => Some(SpecAssert::And(Vec::new())),
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
        ParsedAssert::CounterVarCompare { .. } | ParsedAssert::StringNonEmpty { .. } => {
            Some(SpecAssert::RuntimeEnforced {
                enforcement_version: temper_spec::automaton::RUNTIME_INVARIANT_ENFORCEMENT_VERSION,
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
        self.state = super::effects::build_initial_entity_state(
            &self.state.entity_type,
            &self.state.entity_id,
            &self.table,
            &self.initial_fields,
        )?;
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
                self.state.push_event_bounded(event);
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
        self.state.booleans.get(var).copied()
    }

    fn pending_callbacks(&self) -> Vec<String> {
        self.last_custom_effects.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::scheduler::install_deterministic_context;

    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    fn order_table() -> Arc<TransitionTable> {
        Arc::new(TransitionTable::from_ioa_source(ORDER_IOA))
    }

    #[test]
    fn handler_starts_in_draft() {
        let (_guard, _clock, _id_gen) = install_deterministic_context(42);
        let mut handler = EntityActorHandler::new("Order", "o1", order_table());
        handler.init().unwrap();
        assert_eq!(handler.current_status(), "Draft");
        assert_eq!(handler.current_item_count(), 0);
        assert_eq!(handler.event_count(), 0);
    }

    #[test]
    fn handler_add_item_then_submit() {
        let (_guard, clock, _id_gen) = install_deterministic_context(42);
        let mut handler = EntityActorHandler::new("Order", "o1", order_table());
        handler.init().unwrap();

        // AddItem
        clock.advance();
        let result = handler.handle_message("AddItem", r#"{"ProductId":"laptop"}"#);
        assert!(result.is_ok());
        assert_eq!(handler.current_status(), "Draft");
        assert_eq!(handler.current_item_count(), 1);
        assert_eq!(handler.event_count(), 1);

        // SubmitOrder
        clock.advance();
        let result = handler.handle_message("SubmitOrder", "{}");
        assert!(result.is_ok());
        assert_eq!(handler.current_status(), "Submitted");
        assert_eq!(handler.event_count(), 2);
    }

    #[test]
    fn handler_cannot_submit_empty() {
        let (_guard, _clock, _id_gen) = install_deterministic_context(42);
        let mut handler = EntityActorHandler::new("Order", "o1", order_table());
        handler.init().unwrap();

        let result = handler.handle_message("SubmitOrder", "{}");
        assert!(result.is_err());
        assert_eq!(handler.current_status(), "Draft");
    }

    #[test]
    fn handler_valid_actions_from_draft() {
        let (_guard, _clock, _id_gen) = install_deterministic_context(42);
        let mut handler = EntityActorHandler::new("Order", "o1", order_table());
        handler.init().unwrap();

        let actions = handler.valid_actions();
        assert!(actions.contains(&"AddItem".to_string()), "got: {actions:?}");
        assert!(
            actions.contains(&"CancelOrder".to_string()),
            "got: {actions:?}"
        );
        // SubmitOrder requires items > 0, so not valid with 0 items
        assert!(
            !actions.contains(&"SubmitOrder".to_string()),
            "got: {actions:?}"
        );
    }

    #[test]
    fn handler_valid_actions_after_add_item() {
        let (_guard, clock, _id_gen) = install_deterministic_context(42);
        let mut handler = EntityActorHandler::new("Order", "o1", order_table());
        handler.init().unwrap();

        clock.advance();
        handler.handle_message("AddItem", "{}").unwrap();

        let actions = handler.valid_actions();
        assert!(actions.contains(&"AddItem".to_string()));
        assert!(
            actions.contains(&"SubmitOrder".to_string()),
            "got: {actions:?}"
        );
        assert!(
            actions.contains(&"RemoveItem".to_string()),
            "got: {actions:?}"
        );
    }

    #[test]
    fn handler_with_ioa_invariants_parses_spec() {
        let (_guard, _clock, _id_gen) = install_deterministic_context(42);
        let handler =
            EntityActorHandler::new("Order", "o1", order_table()).with_ioa_invariants(ORDER_IOA);

        let invariants = handler.spec_invariants();
        assert!(
            !invariants.is_empty(),
            "should have parsed invariants from IOA spec"
        );

        let names: Vec<&str> = invariants.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"SubmitRequiresItems"),
            "should have SubmitRequiresItems, got: {names:?}"
        );
        assert!(
            names.contains(&"CancelledIsFinal"),
            "should have CancelledIsFinal, got: {names:?}"
        );
        assert!(names.contains(&"ShipRequiresPayment"));
    }

    #[test]
    fn handler_without_ioa_invariants_returns_empty() {
        let (_guard, _clock, _id_gen) = install_deterministic_context(42);
        let handler = EntityActorHandler::new("Order", "o1", order_table());

        assert!(handler.spec_invariants().is_empty());
    }
}

#[cfg(test)]
#[path = "sim_handler/runtime_invariant_tests.rs"]
mod runtime_invariant_tests;
