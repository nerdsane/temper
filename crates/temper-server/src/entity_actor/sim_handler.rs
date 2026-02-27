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

        let state = EntityState {
            entity_type,
            entity_id,
            status: table.initial_state.clone(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: Vec::new(),
            sequence_nr: 0,
        };

        Self {
            table,
            state,
            invariants: Vec::new(),
            last_custom_effects: Vec::new(),
            last_scheduled_actions: Vec::new(),
        }
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

        self.invariants = automaton
            .invariants
            .iter()
            .filter_map(|inv| {
                let assert_kind = parse_assert_expr(&inv.assert)?;
                Some(SpecInvariant {
                    name: inv.name.clone(),
                    when: inv.when.clone(),
                    assert: assert_kind,
                })
            })
            .collect();

        self
    }
}

/// Parse an assertion expression from the IOA spec into a [`SpecAssert`].
///
/// Returns `None` for expressions that the framework cannot check automatically.
fn parse_assert_expr(expr: &str) -> Option<SpecAssert> {
    let trimmed = expr.trim();

    // Pattern: "items > 0" or "var > 0" — shorthand for CounterPositive.
    if trimmed.contains("> 0") && !trimmed.contains(">=") {
        let var = trimmed.split('>').next()?.trim().to_string();
        return Some(SpecAssert::CounterPositive { var });
    }

    // Pattern: "no_further_transitions"
    if trimmed == "no_further_transitions" {
        return Some(SpecAssert::NoFurtherTransitions);
    }

    // Pattern: "ordering(StateA, StateB)" — StateA must precede StateB.
    if trimmed.starts_with("ordering(") && trimmed.ends_with(')') {
        let inner = &trimmed[9..trimmed.len() - 1];
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        if parts.len() == 2 {
            return Some(SpecAssert::OrderingConstraint {
                before: parts[0].to_string(),
                after: parts[1].to_string(),
            });
        }
    }

    // Pattern: "never(StateName)" — entity should never be in this state.
    if trimmed.starts_with("never(") && trimmed.ends_with(')') {
        let state = trimmed[6..trimmed.len() - 1].trim().to_string();
        return Some(SpecAssert::NeverState { state });
    }

    // Generalized counter comparison: "var >= N", "var <= N", "var == N",
    // "var > N", "var < N". Order matters: check two-char ops before one-char.
    let ops: &[(&str, CompareOp)] = &[
        (">=", CompareOp::Gte),
        ("<=", CompareOp::Lte),
        ("==", CompareOp::Eq),
        (">", CompareOp::Gt),
        ("<", CompareOp::Lt),
    ];
    for (op_str, op) in ops {
        if let Some(pos) = trimmed.find(op_str) {
            let var = trimmed[..pos].trim().to_string();
            let val_str = trimmed[pos + op_str.len()..].trim();
            if let Ok(value) = val_str.parse::<usize>() {
                // "var > 0" is already handled by CounterPositive above.
                if *op_str == ">" && value == 0 {
                    continue;
                }
                return Some(SpecAssert::CounterCompare {
                    var,
                    op: op.clone(),
                    value,
                });
            }
        }
    }

    // Unrecognized expression — caller needs a manual checker.
    None
}

impl SimActorHandler for EntityActorHandler {
    fn init(&mut self) -> Result<serde_json::Value, String> {
        // Reset to initial state
        self.state.status = self.table.initial_state.clone();
        self.state.item_count = 0;
        self.state.counters.clear();
        self.state.booleans.clear();
        self.state.lists.clear();
        self.state.events.clear();
        self.state.sequence_nr = 0;
        self.state.fields = serde_json::json!({
            "Id": self.state.entity_id,
            "Status": self.state.status,
        });

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
                self.state.events.push(event);
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
        self.state.events.len()
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
    }

    #[test]
    fn handler_without_ioa_invariants_returns_empty() {
        let (_guard, _clock, _id_gen) = install_deterministic_context(42);
        let handler = EntityActorHandler::new("Order", "o1", order_table());

        assert!(handler.spec_invariants().is_empty());
    }
}
