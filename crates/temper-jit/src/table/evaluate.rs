//! Transition evaluation logic.
//!
//! Evaluates whether a transition can fire given the current runtime context
//! and computes the resulting state.

use super::types::{EvalContext, TransitionResult, TransitionTable};

impl TransitionTable {
    /// Evaluate whether a transition can fire (legacy API with single item count).
    ///
    /// For multi-counter or boolean guard support, use
    /// [`evaluate_ctx()`](Self::evaluate_ctx).
    pub fn evaluate(
        &self,
        current_state: &str,
        item_count: usize,
        action: &str,
    ) -> Option<TransitionResult> {
        let mut ctx = EvalContext::default();
        ctx.counters.insert("items".to_string(), item_count);
        self.evaluate_ctx(current_state, &ctx, action)
    }

    /// Evaluate whether a transition can fire with a full evaluation context.
    ///
    /// Returns `Some(TransitionResult)` with `success: true` if a matching rule
    /// is found and its guard passes, or `Some(TransitionResult)` with
    /// `success: false` if a rule matches by name but its guard fails.
    /// Returns `None` if no rule with the given `action` name exists.
    pub fn evaluate_ctx(
        &self,
        current_state: &str,
        ctx: &EvalContext,
        action: &str,
    ) -> Option<TransitionResult> {
        let matching: Vec<_> =
            self.rules.iter().filter(|r| r.name == action).collect();

        if matching.is_empty() {
            return None;
        }

        for rule in &matching {
            let state_ok = rule.from_states.is_empty()
                || rule.from_states.iter().any(|s| s == current_state);

            if !state_ok {
                continue;
            }

            if !rule.guard.check(current_state, ctx) {
                return Some(TransitionResult {
                    new_state: current_state.to_string(),
                    effects: vec![],
                    success: false,
                });
            }

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
#[allow(deprecated)] // Tests exercise the legacy from_state_machine() path.
mod tests {
    use super::super::types::*;
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
    // Test 2: Valid transition -- Draft + SubmitOrder -> Submitted
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
    // Test 3: Invalid transition -- Shipped + AddItem -> None
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
    // Test 5: Guard evaluation -- StateIn
    // ------------------------------------------------------------------
    #[test]
    fn guard_state_in() {
        let guard = Guard::StateIn(vec!["Draft".into(), "Submitted".into()]);
        assert!(guard.evaluate("Draft", 0));
        assert!(guard.evaluate("Submitted", 0));
        assert!(!guard.evaluate("Shipped", 0));
    }

    // ------------------------------------------------------------------
    // Test 6: Guard evaluation -- ItemCountMin
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
    // Test 7: Guard evaluation -- And combinator
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

#[cfg(test)]
mod tla_tests {
    use super::super::types::TransitionTable;

    #[test]
    fn debug_tla_table() {
        let tla = include_str!("../../../../test-fixtures/specs/order.tla");
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
}
