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
    ///
    /// Uses a pre-built index for O(log K) action lookup instead of a linear
    /// scan, eliminating the Vec allocation on the hot path.
    pub fn evaluate_ctx(
        &self,
        current_state: &str,
        ctx: &EvalContext,
        action: &str,
    ) -> Option<TransitionResult> {
        let indices = match self.rule_index.get(action) {
            Some(idx) => idx,
            None => return None,
        };

        for &i in indices {
            let rule = &self.rules[i];

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
mod tests {
    use super::super::types::*;

    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    fn order_table() -> TransitionTable {
        TransitionTable::from_ioa_source(ORDER_IOA)
    }

    #[test]
    fn build_table_from_ioa() {
        let table = order_table();
        assert_eq!(table.entity_name, "Order");
        assert_eq!(table.initial_state, "Draft");
        assert_eq!(table.states.len(), 10);
    }

    #[test]
    fn evaluate_valid_submit_order() {
        let table = order_table();
        let result = table.evaluate("Draft", 2, "SubmitOrder");
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.success);
        assert_eq!(r.new_state, "Submitted");
        assert!(r.effects.contains(&Effect::SetState("Submitted".into())));
        assert!(r.effects.contains(&Effect::EmitEvent("SubmitOrder".into())));
    }

    #[test]
    fn evaluate_invalid_shipped_add_item() {
        let table = order_table();
        let result = table.evaluate("Shipped", 3, "AddItem");
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(!r.success, "AddItem should fail from Shipped state");
        assert_eq!(r.new_state, "Shipped");
    }

    #[test]
    fn evaluate_unknown_action() {
        let table = order_table();
        let result = table.evaluate("Draft", 0, "DoSomethingUnknown");
        assert!(result.is_none());
    }

    #[test]
    fn guard_state_in() {
        let guard = Guard::StateIn(vec!["Draft".into(), "Submitted".into()]);
        assert!(guard.evaluate("Draft", 0));
        assert!(guard.evaluate("Submitted", 0));
        assert!(!guard.evaluate("Shipped", 0));
    }

    #[test]
    fn guard_item_count_min() {
        let guard = Guard::ItemCountMin(3);
        assert!(!guard.evaluate("Draft", 0));
        assert!(!guard.evaluate("Draft", 2));
        assert!(guard.evaluate("Draft", 3));
        assert!(guard.evaluate("Draft", 10));
    }

    #[test]
    fn guard_and_combinator() {
        let guard = Guard::And(vec![
            Guard::StateIn(vec!["Draft".into()]),
            Guard::ItemCountMin(1),
        ]);

        assert!(guard.evaluate("Draft", 2));
        assert!(!guard.evaluate("Shipped", 2));
        assert!(!guard.evaluate("Draft", 0));
        assert!(!guard.evaluate("Shipped", 0));
    }

    #[test]
    fn test_serde_roundtrip_preserves_rule_index() {
        let table = order_table();

        // Serialize → deserialize roundtrip
        let json = serde_json::to_string(&table).expect("serialize");
        let restored: TransitionTable =
            serde_json::from_str(&json).expect("deserialize");

        // rule_index must be rebuilt, not empty
        assert!(
            !restored.rule_index.is_empty(),
            "rule_index should be non-empty after deserialization"
        );

        // Evaluate must still work on the deserialized table
        let result = restored.evaluate("Draft", 2, "SubmitOrder");
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.success, "SubmitOrder from Draft should succeed after roundtrip");
        assert_eq!(r.new_state, "Submitted");
    }

    #[test]
    fn cancel_from_multiple_states() {
        let table = order_table();

        let r = table.evaluate("Draft", 0, "CancelOrder").unwrap();
        assert!(r.success);
        assert_eq!(r.new_state, "Cancelled");

        let r = table.evaluate("Submitted", 1, "CancelOrder").unwrap();
        assert!(r.success);
        assert_eq!(r.new_state, "Cancelled");

        let r = table.evaluate("Shipped", 1, "CancelOrder").unwrap();
        assert!(!r.success);
    }
}
