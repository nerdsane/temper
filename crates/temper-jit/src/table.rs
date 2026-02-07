//! Transition tables: state machine transitions as DATA, not code.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type. It can be built from a TLA+ [`StateMachine`] spec and evaluated
//! at runtime without any compiled transition logic.

use serde::{Deserialize, Serialize};
use stateright::Model as _;
use temper_spec::tlaplus::StateMachine;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A transition table: state machine transitions as DATA, not code.
/// Can be hot-swapped per-actor without restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionTable {
    /// The entity this table governs (e.g. "Order").
    pub entity_name: String,
    /// All valid state values.
    pub states: Vec<String>,
    /// The state an entity starts in.
    pub initial_state: String,
    /// Ordered list of transition rules.
    pub rules: Vec<TransitionRule>,
}

/// A single transition rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRule {
    /// Action name (e.g. "SubmitOrder").
    pub name: String,
    /// States this transition may fire from.
    pub from_states: Vec<String>,
    /// Target state after the transition (if deterministic).
    pub to_state: Option<String>,
    /// Guard condition evaluated before the transition fires.
    pub guard: Guard,
    /// Effects applied after the transition fires.
    pub effects: Vec<Effect>,
}

/// A guard condition (evaluated before a transition fires).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Guard {
    /// No guard -- always passes.
    Always,
    /// Current state must be in the given set.
    StateIn(Vec<String>),
    /// `items.len() >= N`.
    ItemCountMin(usize),
    /// All inner guards must pass.
    And(Vec<Guard>),
}

/// An effect applied after a transition fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Effect {
    /// Change the entity status.
    SetState(String),
    /// Add an item (increment item count).
    IncrementItems,
    /// Remove an item (decrement item count).
    DecrementItems,
    /// Emit a named event.
    EmitEvent(String),
}

/// The result of evaluating a transition.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionResult {
    /// The new state after the transition (may be unchanged).
    pub new_state: String,
    /// Effects that were applied.
    pub effects: Vec<Effect>,
    /// Whether the transition succeeded.
    pub success: bool,
}

// ---------------------------------------------------------------------------
// Guard evaluation
// ---------------------------------------------------------------------------

impl Guard {
    /// Evaluate this guard against the current runtime context.
    pub fn evaluate(&self, current_state: &str, item_count: usize) -> bool {
        match self {
            Guard::Always => true,
            Guard::StateIn(states) => states.iter().any(|s| s == current_state),
            Guard::ItemCountMin(n) => item_count >= *n,
            Guard::And(guards) => guards.iter().all(|g| g.evaluate(current_state, item_count)),
        }
    }
}

// ---------------------------------------------------------------------------
// TransitionTable construction
// ---------------------------------------------------------------------------

impl TransitionTable {
    /// Build a [`TransitionTable`] from a TLA+ [`StateMachine`] specification.
    ///
    /// Each [`Transition`](temper_spec::tlaplus::Transition) in the spec is
    /// converted into a [`TransitionRule`] with:
    /// - A `StateIn` guard derived from `from_states`.
    /// - A `SetState` effect derived from `to_state`.
    /// - An `EmitEvent` effect for every transition (using the action name).
    pub fn from_state_machine(sm: &StateMachine) -> Self {
        let rules = sm
            .transitions
            .iter()
            .map(|t| {
                // Build the guard --------------------------------------------------
                let guard = if t.from_states.is_empty() {
                    Guard::Always
                } else {
                    Guard::StateIn(t.from_states.clone())
                };

                // Build effects ----------------------------------------------------
                let mut effects: Vec<Effect> = Vec::new();

                if let Some(ref to) = t.to_state {
                    effects.push(Effect::SetState(to.clone()));
                }

                // Derive additional effects from the raw effect expression.
                let expr = t.effect_expr.to_lowercase();
                if expr.contains("items' = items \\union") || expr.contains("items' = items \\cup") {
                    effects.push(Effect::IncrementItems);
                }
                if expr.contains("items' = items \\") && expr.contains("\\{") {
                    // set difference pattern: items' = items \ {item}
                    // already handled below
                }
                if expr.contains("items' = items \\") && !expr.contains("union") && !expr.contains("cup") {
                    effects.push(Effect::DecrementItems);
                }

                // Always emit an event named after the action.
                effects.push(Effect::EmitEvent(t.name.clone()));

                TransitionRule {
                    name: t.name.clone(),
                    from_states: t.from_states.clone(),
                    to_state: t.to_state.clone(),
                    guard,
                    effects,
                }
            })
            .collect();

        // Determine initial state: first state in the list, or "Draft" as fallback.
        let initial_state = sm
            .states
            .first()
            .cloned()
            .unwrap_or_else(|| "Draft".to_string());

        TransitionTable {
            entity_name: sm.module_name.clone(),
            states: sm.states.clone(),
            initial_state,
            rules,
        }
    }

    /// Build a TransitionTable from raw TLA+ source with full guard resolution.
    ///
    /// This resolves `CanXxx` predicates by parsing their definitions from the
    /// source, producing correct `from_states` and `requires_items` constraints.
    /// This is the constructor that should be used in production — it matches
    /// what the Stateright model checker and DST simulation verify.
    pub fn from_tla_source(tla_source: &str) -> Self {
        let model: temper_verify::TemperModel = temper_verify::build_model_from_tla(tla_source, 3);

        // Build transition rules directly from the verified model's resolved transitions.
        // These have correct from_states and requires_items from CanXxx guard resolution.
        let rules: Vec<TransitionRule> = model.transitions.iter().map(|rt| {
            let mut effects: Vec<Effect> = Vec::new();
            if let Some(ref target) = rt.to_state {
                effects.push(Effect::SetState(target.clone()));
            }
            if rt.is_add_item {
                effects.push(Effect::IncrementItems);
            }
            if rt.modifies_items && !rt.is_add_item {
                effects.push(Effect::DecrementItems);
            }
            effects.push(Effect::EmitEvent(rt.name.clone()));

            // Build guard from resolved constraints
            let mut guards = vec![];
            if !rt.from_states.is_empty() {
                guards.push(Guard::StateIn(rt.from_states.clone()));
            }
            if rt.requires_items {
                guards.push(Guard::ItemCountMin(1));
            }

            let guard = match guards.len() {
                0 => Guard::Always,
                1 => guards.into_iter().next().unwrap(),
                _ => Guard::And(guards),
            };

            TransitionRule {
                name: rt.name.clone(),
                from_states: rt.from_states.clone(),
                to_state: rt.to_state.clone(),
                guard,
                effects,
            }
        }).collect();

        let initial_state = if model.states.contains(&"Draft".to_string()) {
            "Draft".to_string()
        } else {
            model.states.first().cloned().unwrap_or_default()
        };

        TransitionTable {
            entity_name: "Entity".to_string(),
            states: model.states.clone(),
            initial_state,
            rules,
        }
    }

    /// Evaluate whether a transition can fire given the current runtime context.
    ///
    /// Returns `Some(TransitionResult)` with `success: true` if a matching rule
    /// is found and its guard passes, or `Some(TransitionResult)` with
    /// `success: false` if a rule matches by name but its guard fails.
    /// Returns `None` if no rule with the given `action` name exists.
    pub fn evaluate(
        &self,
        current_state: &str,
        item_count: usize,
        action: &str,
    ) -> Option<TransitionResult> {
        // Find all rules that match the action name.
        let matching: Vec<&TransitionRule> =
            self.rules.iter().filter(|r| r.name == action).collect();

        if matching.is_empty() {
            return None;
        }

        for rule in &matching {
            // Check from_states constraint first.
            let state_ok = rule.from_states.is_empty()
                || rule.from_states.iter().any(|s| s == current_state);

            if !state_ok {
                continue;
            }

            // Evaluate the guard.
            if !rule.guard.evaluate(current_state, item_count) {
                return Some(TransitionResult {
                    new_state: current_state.to_string(),
                    effects: vec![],
                    success: false,
                });
            }

            // Guard passed -- compute result.
            let new_state = rule
                .to_state
                .clone()
                .unwrap_or_else(|| current_state.to_string());

            return Some(TransitionResult {
                new_state,
                effects: rule.effects.clone(),
                success: true,
            });
        }

        // A rule exists but no from_states matched (guard effectively failed).
        Some(TransitionResult {
            new_state: current_state.to_string(),
            effects: vec![],
            success: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::tlaplus::{StateMachine, Transition};

    /// Helper: build a reference Order state machine similar to order.tla.
    fn order_state_machine() -> StateMachine {
        StateMachine {
            module_name: "Order".into(),
            states: vec![
                "Draft".into(),
                "Submitted".into(),
                "Confirmed".into(),
                "Processing".into(),
                "Shipped".into(),
                "Delivered".into(),
                "Cancelled".into(),
                "ReturnRequested".into(),
                "Returned".into(),
                "Refunded".into(),
            ],
            transitions: vec![
                Transition {
                    name: "AddItem".into(),
                    from_states: vec!["Draft".into()],
                    to_state: None,
                    guard_expr: "status = \"Draft\" /\\ Cardinality(items) < MAX_ITEMS".into(),
                    has_parameters: true,
                    effect_expr: "items' = items \\union {item}".into(),
                },
                Transition {
                    name: "RemoveItem".into(),
                    from_states: vec!["Draft".into()],
                    to_state: None,
                    guard_expr: "status = \"Draft\" /\\ item \\in items".into(),
                    has_parameters: true,
                    effect_expr: "items' = items \\ {item}".into(),
                },
                Transition {
                    name: "SubmitOrder".into(),
                    from_states: vec!["Draft".into()],
                    to_state: Some("Submitted".into()),
                    guard_expr: "status = \"Draft\" /\\ Cardinality(items) > 0 /\\ has_address = TRUE".into(),
                    has_parameters: false,
                    effect_expr: "status' = \"Submitted\"".into(),
                },
                Transition {
                    name: "ConfirmOrder".into(),
                    from_states: vec!["Submitted".into()],
                    to_state: Some("Confirmed".into()),
                    guard_expr: "status = \"Submitted\" /\\ payment_status = \"Authorized\"".into(),
                    has_parameters: false,
                    effect_expr: "status' = \"Confirmed\"".into(),
                },
                Transition {
                    name: "ProcessOrder".into(),
                    from_states: vec!["Confirmed".into()],
                    to_state: Some("Processing".into()),
                    guard_expr: "status = \"Confirmed\"".into(),
                    has_parameters: false,
                    effect_expr: "status' = \"Processing\"".into(),
                },
                Transition {
                    name: "ShipOrder".into(),
                    from_states: vec!["Processing".into()],
                    to_state: Some("Shipped".into()),
                    guard_expr: "status = \"Processing\" /\\ payment_status = \"Captured\"".into(),
                    has_parameters: false,
                    effect_expr: "status' = \"Shipped\"".into(),
                },
                Transition {
                    name: "DeliverOrder".into(),
                    from_states: vec!["Shipped".into()],
                    to_state: Some("Delivered".into()),
                    guard_expr: "status = \"Shipped\"".into(),
                    has_parameters: false,
                    effect_expr: "status' = \"Delivered\"".into(),
                },
                Transition {
                    name: "CancelOrder".into(),
                    from_states: vec!["Draft".into(), "Submitted".into(), "Confirmed".into()],
                    to_state: Some("Cancelled".into()),
                    guard_expr: "status \\in {\"Draft\", \"Submitted\", \"Confirmed\"}".into(),
                    has_parameters: true,
                    effect_expr: "status' = \"Cancelled\"".into(),
                },
                Transition {
                    name: "InitiateReturn".into(),
                    from_states: vec!["Shipped".into(), "Delivered".into()],
                    to_state: Some("ReturnRequested".into()),
                    guard_expr: "status \\in {\"Shipped\", \"Delivered\"}".into(),
                    has_parameters: true,
                    effect_expr: "status' = \"ReturnRequested\"".into(),
                },
                Transition {
                    name: "CompleteReturn".into(),
                    from_states: vec!["ReturnRequested".into()],
                    to_state: Some("Returned".into()),
                    guard_expr: "status = \"ReturnRequested\"".into(),
                    has_parameters: false,
                    effect_expr: "status' = \"Returned\"".into(),
                },
                Transition {
                    name: "RefundOrder".into(),
                    from_states: vec!["Returned".into()],
                    to_state: Some("Refunded".into()),
                    guard_expr: "status = \"Returned\" /\\ payment_status \\in {\"Captured\", \"PartiallyRefunded\"}".into(),
                    has_parameters: false,
                    effect_expr: "status' = \"Refunded\"".into(),
                },
            ],
            invariants: vec![],
            liveness_properties: vec![],
            constants: vec!["MAX_ITEMS".into(), "MAX_ORDER_TOTAL".into()],
            variables: vec!["status".into(), "items".into(), "total".into()],
        }
    }

    // ------------------------------------------------------------------
    // Test 1: Build TransitionTable from reference Order StateMachine
    // ------------------------------------------------------------------
    #[test]
    fn build_table_from_state_machine() {
        let sm = order_state_machine();
        let table = TransitionTable::from_state_machine(&sm);

        assert_eq!(table.entity_name, "Order");
        assert_eq!(table.initial_state, "Draft");
        assert_eq!(table.states.len(), 10);
        assert_eq!(table.rules.len(), 11); // 11 transitions in the helper
    }

    // ------------------------------------------------------------------
    // Test 2: Valid transition — Draft + SubmitOrder -> Submitted
    // ------------------------------------------------------------------
    #[test]
    fn evaluate_valid_submit_order() {
        let sm = order_state_machine();
        let table = TransitionTable::from_state_machine(&sm);

        let result = table.evaluate("Draft", 2, "SubmitOrder");
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.success);
        assert_eq!(r.new_state, "Submitted");
        assert!(r.effects.contains(&Effect::SetState("Submitted".into())));
        assert!(r.effects.contains(&Effect::EmitEvent("SubmitOrder".into())));
    }

    // ------------------------------------------------------------------
    // Test 3: Invalid transition — Shipped + AddItem -> None
    // ------------------------------------------------------------------
    #[test]
    fn evaluate_invalid_shipped_add_item() {
        let sm = order_state_machine();
        let table = TransitionTable::from_state_machine(&sm);

        let result = table.evaluate("Shipped", 3, "AddItem");
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(!r.success, "AddItem should fail from Shipped state");
        assert_eq!(r.new_state, "Shipped");
    }

    // ------------------------------------------------------------------
    // Test 4: Unknown action returns None
    // ------------------------------------------------------------------
    #[test]
    fn evaluate_unknown_action() {
        let sm = order_state_machine();
        let table = TransitionTable::from_state_machine(&sm);

        let result = table.evaluate("Draft", 0, "DoSomethingUnknown");
        assert!(result.is_none());
    }

    // ------------------------------------------------------------------
    // Test 5: Guard evaluation — StateIn
    // ------------------------------------------------------------------
    #[test]
    fn guard_state_in() {
        let guard = Guard::StateIn(vec!["Draft".into(), "Submitted".into()]);
        assert!(guard.evaluate("Draft", 0));
        assert!(guard.evaluate("Submitted", 0));
        assert!(!guard.evaluate("Shipped", 0));
    }

    // ------------------------------------------------------------------
    // Test 6: Guard evaluation — ItemCountMin
    // ------------------------------------------------------------------
    #[test]
    fn guard_item_count_min() {
        let guard = Guard::ItemCountMin(3);
        assert!(!guard.evaluate("Draft", 0));
        assert!(!guard.evaluate("Draft", 2));
        assert!(guard.evaluate("Draft", 3));
        assert!(guard.evaluate("Draft", 10));
    }

    // ------------------------------------------------------------------
    // Test 7: Guard evaluation — And combinator
    // ------------------------------------------------------------------
    #[test]
    fn guard_and_combinator() {
        let guard = Guard::And(vec![
            Guard::StateIn(vec!["Draft".into()]),
            Guard::ItemCountMin(1),
        ]);

        // Both pass.
        assert!(guard.evaluate("Draft", 2));
        // State wrong.
        assert!(!guard.evaluate("Shipped", 2));
        // Count too low.
        assert!(!guard.evaluate("Draft", 0));
        // Both fail.
        assert!(!guard.evaluate("Shipped", 0));
    }

    // ------------------------------------------------------------------
    // Test 8: CancelOrder from multiple from_states
    // ------------------------------------------------------------------
    #[test]
    fn cancel_from_multiple_states() {
        let sm = order_state_machine();
        let table = TransitionTable::from_state_machine(&sm);

        // Cancel from Draft
        let r = table.evaluate("Draft", 0, "CancelOrder").unwrap();
        assert!(r.success);
        assert_eq!(r.new_state, "Cancelled");

        // Cancel from Submitted
        let r = table.evaluate("Submitted", 1, "CancelOrder").unwrap();
        assert!(r.success);
        assert_eq!(r.new_state, "Cancelled");

        // Cancel from Shipped should fail
        let r = table.evaluate("Shipped", 1, "CancelOrder").unwrap();
        assert!(!r.success);
    }
}

#[test]
fn debug_tla_table() {
    let tla = include_str!("../../../test-fixtures/specs/order.tla");
    let table = TransitionTable::from_tla_source(tla);
    for rule in &table.rules {
        eprintln!("{}: from_states={:?} to={:?} guard={:?}", rule.name, rule.from_states, rule.to_state, rule.guard);
    }
    // Find CancelOrder
    let cancel = table.rules.iter().find(|r| r.name == "CancelOrder");
    eprintln!("\nCancelOrder: {:?}", cancel);
    // Try evaluate
    let r = table.evaluate("Draft", 0, "CancelOrder");
    eprintln!("Evaluate Draft+CancelOrder: {:?}", r);
    let r = table.evaluate("Shipped", 1, "CancelOrder");
    eprintln!("Evaluate Shipped+CancelOrder: {:?}", r);
}
