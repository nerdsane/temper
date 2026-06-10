//! Cross-interpreter guard equivalence.
//!
//! `temper-jit` (production) and `temper-verify` (model checking) both build
//! their guard types from the shared `temper-spec` translation layer, but each
//! *evaluates* guards with its own interpreter (`Guard::check` vs
//! `semantics::evaluate_guard`). Any semantic drift between the two silently
//! invalidates verification results: the cascade would prove properties about
//! behavior the runtime doesn't have.
//!
//! This test drives both interpreters from one spec across an exhaustive state
//! corpus and requires them to agree on every verdict.
//!
//! `cross_entity_state` guards are deliberately excluded: temper-verify maps
//! them to `Always` (permissive) while temper-jit resolves them at runtime, so
//! they are not comparable here by design.

use std::collections::BTreeMap;

use temper_jit::{EvalContext, TransitionTable};

use super::builder::build_model_from_ioa;
use super::semantics::evaluate_guard;
use super::types::TemperModelState;

/// One action per guard form shared by both interpreters, plus a conjunction.
const SPEC: &str = r#"
[automaton]
name = "Equivalence"
states = ["A", "B"]
initial = "A"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[state]]
name = "retries"
type = "counter"
initial = "0"

[[state]]
name = "ready"
type = "bool"
initial = "false"

[[state]]
name = "tags"
type = "list"

[[action]]
name = "Unguarded"
from = ["A"]
to = "B"

[[action]]
name = "NeedsItems"
from = ["A"]
to = "B"
guard = [{ type = "min_count", var = "items", min = 2 }]

[[action]]
name = "UnderRetryCap"
from = ["A", "B"]
to = "B"
guard = [{ type = "max_count", var = "retries", max = 3 }]

[[action]]
name = "WhenReady"
from = ["A"]
to = "B"
guard = [{ type = "is_true", var = "ready" }]

[[action]]
name = "WhenNotReady"
from = ["A"]
to = "B"
guard = [{ type = "is_false", var = "ready" }]

[[action]]
name = "VipOnly"
from = ["A"]
to = "B"
guard = [{ type = "list_contains", var = "tags", value = "vip" }]

[[action]]
name = "TwoTags"
from = ["A"]
to = "B"
guard = [{ type = "list_length_min", var = "tags", min = 2 }]

[[action]]
name = "Conjunction"
from = ["B"]
to = "A"
guard = [
    { type = "min_count", var = "items", min = 1 },
    { type = "max_count", var = "items", max = 4 },
    { type = "is_true", var = "ready" },
]
"#;

/// Every combination of status, counter values around the guard thresholds,
/// boolean values, and list shapes the spec's guards can distinguish.
fn state_corpus() -> Vec<TemperModelState> {
    let tag_shapes: [&[&str]; 4] = [&[], &["basic"], &["vip"], &["vip", "basic"]];
    let mut corpus = Vec::new();
    for status in ["A", "B"] {
        for items in 0..=5usize {
            for retries in 0..=4usize {
                for ready in [false, true] {
                    for tags in tag_shapes {
                        corpus.push(TemperModelState {
                            status: status.to_string(),
                            counters: BTreeMap::from([
                                ("items".to_string(), items),
                                ("retries".to_string(), retries),
                            ]),
                            booleans: BTreeMap::from([("ready".to_string(), ready)]),
                            lists: BTreeMap::from([(
                                "tags".to_string(),
                                tags.iter().map(|t| t.to_string()).collect(),
                            )]),
                        });
                    }
                }
            }
        }
    }
    corpus
}

#[test]
fn jit_and_verify_guard_interpreters_agree() {
    let table = TransitionTable::try_from_ioa_source(SPEC).expect("jit table builds");
    let model = build_model_from_ioa(SPEC, 5).expect("verify model builds");
    assert!(
        !model.transitions.is_empty(),
        "spec produced no transitions"
    );

    let mut checked = 0usize;
    for transition in &model.transitions {
        let rule = table
            .rules
            .iter()
            .find(|r| r.name == transition.name)
            .unwrap_or_else(|| panic!("jit table missing rule for '{}'", transition.name));

        for state in state_corpus() {
            let ctx = EvalContext {
                counters: state.counters.clone(),
                booleans: state.booleans.clone(),
                lists: state.lists.clone(),
            };
            let jit_verdict = rule.guard.check(&state.status, &ctx);
            let model_verdict = evaluate_guard(&transition.guard, &state);
            assert_eq!(
                jit_verdict, model_verdict,
                "guard drift on action '{}' in state {state}: jit={jit_verdict}, verify={model_verdict}",
                transition.name,
            );
            checked += 1;
        }
    }
    assert!(checked > 1000, "corpus unexpectedly small: {checked}");
}
