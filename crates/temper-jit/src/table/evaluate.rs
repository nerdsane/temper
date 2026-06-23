//! Transition evaluation logic.
//!
//! Evaluates whether a transition can fire given the current runtime context
//! and computes the resulting state.

use super::guard::EvalContext;
use super::types::{Effect, TransitionResult, TransitionTable};

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
    /// On a guard rejection the result's `guard_failure` names the first
    /// failing sub-guard (ADR-0151). A from-state miss (no rule fires from the
    /// current status) leaves `guard_failure` as `None` — there is no
    /// sub-guard to name, only a wrong-state condition.
    ///
    /// Uses a pre-built index for O(log K) action lookup instead of a linear
    /// scan, eliminating the Vec allocation on the hot path. The detailed
    /// failure walk runs only on the cold rejection path.
    pub fn evaluate_ctx(
        &self,
        current_state: &str,
        ctx: &EvalContext,
        action: &str,
    ) -> Option<TransitionResult> {
        let indices = self.rule_index.get(action)?;

        for &i in indices {
            let rule = &self.rules[i];

            let state_ok =
                rule.from_states.is_empty() || rule.from_states.iter().any(|s| s == current_state);

            if !state_ok {
                continue;
            }

            if !rule.guard.check(current_state, ctx) {
                return Some(TransitionResult {
                    new_state: current_state.to_string(),
                    effects: vec![],
                    success: false,
                    guard_failure: rule.guard.check_detailed(current_state, ctx),
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
                guard_failure: None,
            });
        }

        // From-state miss: a rule with this action name exists, but none fires
        // from the current status. No sub-guard failed — leave `guard_failure`
        // unset so the server renders the generic "not valid from state" error.
        Some(TransitionResult {
            new_state: current_state.to_string(),
            effects: vec![],
            success: false,
            guard_failure: None,
        })
    }

    /// Effects to apply when **replaying** a durably-stored transition.
    ///
    /// A persisted event is a historical fact: its guard already passed at
    /// commit time and the resulting `to_status` is authoritative. Replay must
    /// therefore re-derive the transition's *effects* (counter/bool changes)
    /// from the table **without re-evaluating guards** — in particular,
    /// cross-entity guards cannot be re-evaluated during replay because the
    /// related entity's state is not reconstructed into the eval context, and a
    /// spurious guard "failure" must not silently drop committed history.
    ///
    /// Returns the effects of the first rule matching `action` whose
    /// `from_states` admit `current_state` (the same rule the live dispatch
    /// fired). Returns `None` when no such rule exists (unknown action / from
    /// state) — the caller then relies solely on the stored `to_status`.
    pub fn replay_effects(&self, current_state: &str, action: &str) -> Option<&[Effect]> {
        let indices = self.rule_index.get(action)?;
        for &i in indices {
            let rule = &self.rules[i];
            let state_ok =
                rule.from_states.is_empty() || rule.from_states.iter().any(|s| s == current_state);
            if state_ok {
                return Some(&rule.effects);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::guard::{EvalContext, Guard, GuardFailureKind};
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
    fn guard_counter_max() {
        let guard = Guard::CounterMax {
            var: "retries".into(),
            max: 3,
        };
        let mut ctx = EvalContext::default();
        ctx.counters.insert("retries".into(), 2);
        assert!(guard.check("Draft", &ctx));
        ctx.counters.insert("retries".into(), 3);
        assert!(!guard.check("Draft", &ctx));
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
        let restored: TransitionTable = serde_json::from_str(&json).expect("deserialize");

        // rule_index must be rebuilt, not empty
        assert!(
            !restored.rule_index.is_empty(),
            "rule_index should be non-empty after deserialization"
        );

        // Evaluate must still work on the deserialized table
        let result = restored.evaluate("Draft", 2, "SubmitOrder");
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(
            r.success,
            "SubmitOrder from Draft should succeed after roundtrip"
        );
        assert_eq!(r.new_state, "Submitted");
    }

    #[test]
    fn guard_rejection_carries_guard_failure_identity() {
        let table = order_table();
        // SubmitOrder from Draft requires items > 0; with 0 items the guard
        // (CounterMin on "items") fails.
        let r = table.evaluate("Draft", 0, "SubmitOrder").unwrap();
        assert!(!r.success);
        let failure = r
            .guard_failure
            .expect("guard rejection must carry a GuardFailure");
        assert_eq!(failure.kind, GuardFailureKind::CounterMin);
        assert_eq!(failure.var.as_deref(), Some("items"));
        assert_eq!(failure.found.as_deref(), Some("0"));
    }

    #[test]
    fn from_state_miss_has_no_guard_failure() {
        let table = order_table();
        // AddItem only fires from Draft; from Shipped this is a from-state miss,
        // not a guard rejection, so guard_failure stays None.
        let r = table.evaluate("Shipped", 3, "AddItem").unwrap();
        assert!(!r.success);
        assert!(
            r.guard_failure.is_none(),
            "from-state miss must not carry a GuardFailure"
        );
    }

    #[test]
    fn successful_transition_has_no_guard_failure() {
        let table = order_table();
        let r = table.evaluate("Draft", 2, "SubmitOrder").unwrap();
        assert!(r.success);
        assert!(r.guard_failure.is_none());
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
