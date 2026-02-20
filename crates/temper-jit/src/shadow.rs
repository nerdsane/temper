//! Shadow testing: compare old and new transition tables for observational equivalence.
//!
//! Before hot-swapping a transition table into production, [`shadow_test`] runs a
//! suite of [`TestCase`]s against both the old and the new table and reports any
//! differences in outcome. This gives operators confidence that a swap will not
//! change observable behaviour (or surfaces the exact cases where it does).

use crate::table::{TransitionResult, TransitionTable};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single test scenario: (current_state, item_count, action).
#[derive(Debug, Clone)]
pub struct TestCase {
    /// The current state of the entity.
    pub state: String,
    /// The current item count.
    pub item_count: usize,
    /// The action to attempt.
    pub action: String,
}

/// A mismatch between the old and new table for a single test case.
#[derive(Debug, Clone)]
pub struct Mismatch {
    /// The test case that produced the mismatch.
    pub test_case: TestCase,
    /// Result from the old table.
    pub old_result: Option<TransitionResult>,
    /// Result from the new table.
    pub new_result: Option<TransitionResult>,
}

/// The result of a shadow test run.
#[derive(Debug)]
pub struct ShadowResult {
    /// Number of test cases that produced identical results.
    pub matches: usize,
    /// All test cases that produced different results.
    pub mismatches: Vec<Mismatch>,
}

impl ShadowResult {
    /// Returns `true` if no mismatches were detected.
    pub fn is_equivalent(&self) -> bool {
        self.mismatches.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Shadow test entry point
// ---------------------------------------------------------------------------

/// Compare `old` and `new` transition tables against the provided `test_cases`.
///
/// For each test case the action is evaluated on both tables. If the results
/// differ (different new state, different success flag, or different effects)
/// the case is recorded as a mismatch.
pub fn shadow_test(
    old: &TransitionTable,
    new: &TransitionTable,
    test_cases: &[TestCase],
) -> ShadowResult {
    let mut matches: usize = 0;
    let mut mismatches: Vec<Mismatch> = Vec::new();

    for tc in test_cases {
        let old_result = old.evaluate(&tc.state, tc.item_count, &tc.action);
        let new_result = new.evaluate(&tc.state, tc.item_count, &tc.action);

        if old_result == new_result {
            matches += 1;
        } else {
            mismatches.push(Mismatch {
                test_case: tc.clone(),
                old_result,
                new_result,
            });
        }
    }

    ShadowResult {
        matches,
        mismatches,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{Effect, Guard, TransitionRule, TransitionTable};

    fn base_table() -> TransitionTable {
        let mut table = TransitionTable {
            entity_name: "Order".into(),
            states: vec!["Draft".into(), "Submitted".into(), "Cancelled".into()],
            initial_state: "Draft".into(),
            rules: vec![
                TransitionRule {
                    name: "SubmitOrder".into(),
                    from_states: vec!["Draft".into()],
                    to_state: Some("Submitted".into()),
                    guard: Guard::StateIn(vec!["Draft".into()]),
                    effects: vec![
                        Effect::SetState("Submitted".into()),
                        Effect::EmitEvent("SubmitOrder".into()),
                    ],
                },
                TransitionRule {
                    name: "CancelOrder".into(),
                    from_states: vec!["Draft".into(), "Submitted".into()],
                    to_state: Some("Cancelled".into()),
                    guard: Guard::StateIn(vec!["Draft".into(), "Submitted".into()]),
                    effects: vec![
                        Effect::SetState("Cancelled".into()),
                        Effect::EmitEvent("CancelOrder".into()),
                    ],
                },
            ],
            rule_index: Default::default(),
        };
        table.rebuild_index();
        table
    }

    fn test_cases() -> Vec<TestCase> {
        vec![
            TestCase {
                state: "Draft".into(),
                item_count: 1,
                action: "SubmitOrder".into(),
            },
            TestCase {
                state: "Draft".into(),
                item_count: 0,
                action: "CancelOrder".into(),
            },
            TestCase {
                state: "Submitted".into(),
                item_count: 1,
                action: "CancelOrder".into(),
            },
            TestCase {
                state: "Submitted".into(),
                item_count: 1,
                action: "SubmitOrder".into(),
            },
        ]
    }

    // ------------------------------------------------------------------
    // Test: identical tables produce no mismatches
    // ------------------------------------------------------------------
    #[test]
    fn identical_tables_match() {
        let old = base_table();
        let new = base_table();

        let result = shadow_test(&old, &new, &test_cases());

        assert!(result.is_equivalent());
        assert_eq!(result.matches, 4);
        assert_eq!(result.mismatches.len(), 0);
    }

    // ------------------------------------------------------------------
    // Test: different tables detect mismatch
    // ------------------------------------------------------------------
    #[test]
    fn different_tables_detect_mismatch() {
        let old = base_table();

        // New table: SubmitOrder now goes to "Confirmed" instead of "Submitted".
        let mut new = base_table();
        new.rules[0].to_state = Some("Confirmed".into());
        new.rules[0].effects = vec![
            Effect::SetState("Confirmed".into()),
            Effect::EmitEvent("SubmitOrder".into()),
        ];
        // No rebuild needed — rule names unchanged, just to_state/effects differ.

        let result = shadow_test(&old, &new, &test_cases());

        assert!(!result.is_equivalent());
        // The first test case (Draft -> SubmitOrder) should mismatch.
        assert!(result.mismatches.len() >= 1);

        let mm = &result.mismatches[0];
        assert_eq!(mm.test_case.action, "SubmitOrder");
        assert_eq!(mm.old_result.as_ref().unwrap().new_state, "Submitted");
        assert_eq!(mm.new_result.as_ref().unwrap().new_state, "Confirmed");
    }

    // ------------------------------------------------------------------
    // Test: adding a new rule causes mismatch for previously unknown action
    // ------------------------------------------------------------------
    #[test]
    fn added_rule_detected() {
        let old = base_table();
        let mut new = base_table();

        // Add a rule the old table doesn't have.
        new.rules.push(TransitionRule {
            name: "ExpediteOrder".into(),
            from_states: vec!["Submitted".into()],
            to_state: Some("Submitted".into()),
            guard: Guard::StateIn(vec!["Submitted".into()]),
            effects: vec![Effect::EmitEvent("ExpediteOrder".into())],
        });
        new.rebuild_index();

        let cases = vec![TestCase {
            state: "Submitted".into(),
            item_count: 1,
            action: "ExpediteOrder".into(),
        }];

        let result = shadow_test(&old, &new, &cases);
        assert!(!result.is_equivalent());
        assert_eq!(result.mismatches.len(), 1);

        // Old should be None (no such action), new should succeed.
        assert!(result.mismatches[0].old_result.is_none());
        assert!(result.mismatches[0].new_result.is_some());
    }
}
