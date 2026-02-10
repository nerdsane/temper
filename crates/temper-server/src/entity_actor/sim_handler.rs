//! Simulation handler for entity actors.
//!
//! [`EntityActorHandler`] wraps a real [`TransitionTable`] and [`EntityState`],
//! implementing [`SimActorHandler`] for deterministic simulation. The
//! `handle_message()` method is the synchronous subset of the production
//! `EntityActor::handle()`: same `evaluate()` call, same effect application,
//! same event recording. No async, no persistence, no telemetry.

use std::sync::Arc;

use temper_jit::table::{EvalContext, TransitionTable};
use temper_runtime::scheduler::{sim_now, SimActorHandler, SpecAssert, SpecInvariant};

use super::types::{EntityEvent, EntityState};

/// Simulation handler wrapping a real TransitionTable.
///
/// This is the bridge that lets [`SimActorSystem`] exercise the identical
/// `TransitionTable::evaluate()` path used in production, with deterministic
/// clock and ID generation.
pub struct EntityActorHandler {
    table: Arc<TransitionTable>,
    state: EntityState,
    invariants: Vec<SpecInvariant>,
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
            fields: serde_json::json!({}),
            events: Vec::new(),
            sequence_nr: 0,
        };

        Self {
            table,
            state,
            invariants: Vec::new(),
        }
    }

    /// Build an [`EvalContext`] from the current entity state.
    fn eval_context(&self) -> EvalContext {
        let mut ctx = EvalContext::default();
        ctx.counters.insert("items".to_string(), self.state.item_count);
        for (k, v) in &self.state.counters {
            ctx.counters.insert(k.clone(), *v);
        }
        for (k, v) in &self.state.booleans {
            ctx.booleans.insert(k.clone(), *v);
        }
        ctx
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

    // Pattern: "items > 0" or "var > 0"
    if trimmed.contains("> 0") {
        let var = trimmed.split('>').next()?.trim().to_string();
        return Some(SpecAssert::CounterPositive { var });
    }

    // Pattern: "no_further_transitions"
    if trimmed == "no_further_transitions" {
        return Some(SpecAssert::NoFurtherTransitions);
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
        self.state.events.clear();
        self.state.sequence_nr = 0;
        self.state.fields = serde_json::json!({
            "Id": self.state.entity_id,
            "Status": self.state.status,
        });

        Ok(serde_json::to_value(&self.state).unwrap_or_default())
    }

    fn handle_message(
        &mut self,
        action: &str,
        params: &str,
    ) -> Result<serde_json::Value, String> {
        let params_value: serde_json::Value =
            serde_json::from_str(params).unwrap_or(serde_json::json!({}));

        // Same evaluate() call as production EntityActor::handle()
        let ctx = self.eval_context();
        let result = self.table.evaluate_ctx(&self.state.status, &ctx, action);

        match result {
            Some(transition_result) if transition_result.success => {
                let from_status = self.state.status.clone();
                let to_status = transition_result.new_state.clone();

                // Apply effects — identical logic to production actor.rs
                for effect in &transition_result.effects {
                    match effect {
                        temper_jit::table::Effect::SetState(s) => {
                            self.state.status = s.clone();
                        }
                        temper_jit::table::Effect::IncrementItems => {
                            self.state.item_count += 1;
                            *self.state.counters.entry("items".to_string()).or_default() += 1;
                        }
                        temper_jit::table::Effect::DecrementItems => {
                            self.state.item_count = self.state.item_count.saturating_sub(1);
                            let c = self.state.counters.entry("items".to_string()).or_default();
                            *c = c.saturating_sub(1);
                        }
                        temper_jit::table::Effect::IncrementCounter(var) => {
                            *self.state.counters.entry(var.clone()).or_default() += 1;
                            // Keep legacy item_count in sync.
                            if var == "items" {
                                self.state.item_count += 1;
                            }
                        }
                        temper_jit::table::Effect::DecrementCounter(var) => {
                            let c = self.state.counters.entry(var.clone()).or_default();
                            *c = c.saturating_sub(1);
                            if var == "items" {
                                self.state.item_count = self.state.item_count.saturating_sub(1);
                            }
                        }
                        temper_jit::table::Effect::SetBool { var, value } => {
                            self.state.booleans.insert(var.clone(), *value);
                        }
                        temper_jit::table::Effect::EmitEvent(_) => {
                            // No telemetry in simulation
                        }
                        temper_jit::table::Effect::Custom(_) => {
                            // Custom effects are handled by post-transition hooks
                        }
                    }
                }

                // If no SetState effect, use the transition result's new_state
                if self.state.status == from_status && !to_status.is_empty() {
                    self.state.status = to_status;
                }

                // Update fields: status + action params + counters + booleans
                if let Some(obj) = self.state.fields.as_object_mut() {
                    obj.insert(
                        "Status".to_string(),
                        serde_json::Value::String(self.state.status.clone()),
                    );
                    // Project action params into fields
                    if let Some(p) = params_value.as_object() {
                        for (k, v) in p {
                            obj.insert(k.clone(), v.clone());
                        }
                    }
                    // Sync counters into fields
                    for (k, v) in &self.state.counters {
                        obj.insert(k.clone(), serde_json::Value::Number((*v as u64).into()));
                    }
                    // Sync booleans into fields
                    for (k, v) in &self.state.booleans {
                        obj.insert(k.clone(), serde_json::Value::Bool(*v));
                    }
                }

                // Record event with sim_now() timestamp (deterministic)
                let event = EntityEvent {
                    action: action.to_string(),
                    from_status,
                    to_status: self.state.status.clone(),
                    timestamp: sim_now(),
                    params: params_value,
                };
                self.state.events.push(event);

                Ok(serde_json::to_value(&self.state).unwrap_or_default())
            }
            Some(_) => {
                // Guard failed
                Err(format!(
                    "Action '{}' not valid from state '{}'",
                    action, self.state.status
                ))
            }
            None => Err(format!("Unknown action: {}", action)),
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
        assert!(actions.contains(&"CancelOrder".to_string()), "got: {actions:?}");
        // SubmitOrder requires items > 0, so not valid with 0 items
        assert!(!actions.contains(&"SubmitOrder".to_string()), "got: {actions:?}");
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
        assert!(actions.contains(&"SubmitOrder".to_string()), "got: {actions:?}");
        assert!(actions.contains(&"RemoveItem".to_string()), "got: {actions:?}");
    }

    #[test]
    fn handler_with_ioa_invariants_parses_spec() {
        let (_guard, _clock, _id_gen) = install_deterministic_context(42);
        let handler = EntityActorHandler::new("Order", "o1", order_table())
            .with_ioa_invariants(ORDER_IOA);

        let invariants = handler.spec_invariants();
        assert!(!invariants.is_empty(), "should have parsed invariants from IOA spec");

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
