//! The `absorbing_effect` lint: a literal write that can swallow a concurrent one.
//!
//! Split out of the parent module to stay inside the file-size budget. The
//! check is self-contained: it reads an `Automaton` and nothing else.

use std::collections::{BTreeMap, BTreeSet};

use super::LintFinding;
use crate::automaton::types::Action;
use crate::automaton::{ActionKind, Automaton};
use crate::predicate::{Arg, AssignOp, Effect, Env, Literal, Val, eval};

/// Flag a literal write that can silently absorb a concurrent write, in a
/// state whose invariant polices the variable written.
///
/// This is a structural check for a race the cascade cannot see. Each level
/// reasons about one entity's transitions in isolation; what goes wrong here is
/// two actions legal in the same state disagreeing about who owns a variable,
/// and the losing write vanishing without an error.
///
/// The pattern, and why each clause is load-bearing:
///
///   1. Action `A` transitions to `T` and assigns a **literal** to `v`. A
///      literal is a claim made from nothing -- unlike a `params.`-derived
///      value, it cannot have come from an observation, so it asserts a fact
///      rather than reporting one.
///   2. Some invariant makes `T` *mean* something about `v`. Without this the
///      overwrite is just a write; with it, `A` satisfies the invariant by
///      construction and the invariant stops being evidence of anything.
///   3. Another **input** action `B` is legal in a state `A` fires from and
///      also writes `v`. Input actions are client- and module-triggerable, so
///      `B` genuinely races `A`; an internal action is sequenced by the server.
///
/// Together these say: `B` observes the world and records it in `v`, `A`
/// overwrites `v` with an assertion, and the entity arrives in `T` advertising
/// a property that `B` had just contradicted. The cascade reports nothing,
/// because every individual transition is legal and the invariant holds in
/// every state it can reach -- it holds precisely *because* `A` wrote it.
///
/// A warning rather than an error: the pattern is sometimes deliberate, and
/// which of the three remedies applies is a design decision the message lays
/// out rather than makes.
pub(super) fn lint(automaton: &Automaton, findings: &mut Vec<LintFinding>) {
    for action in &automaton.actions {
        let Some(target) = action.to.as_deref() else {
            continue;
        };
        for var in literal_writes(action) {
            let Some(invariant) = automaton
                .invariants
                .iter()
                .find(|inv| polices_var_in_status(&inv.assert, target, var))
            else {
                continue;
            };
            let sources = effective_from(automaton, action);
            for other in &automaton.actions {
                if other.name == action.name || other.kind != ActionKind::Input {
                    continue;
                }
                if !writes_var(other, var) {
                    continue;
                }
                let overlap: Vec<&str> = effective_from(automaton, other)
                    .intersection(&sources)
                    .copied()
                    .collect();
                if overlap.is_empty() {
                    continue;
                }
                // `B` must leave `A` still enabled, or the two are alternative
                // outcomes rather than a race. A `B` that transitions out of
                // every shared state removes `A`'s own precondition by firing,
                // so at most one of them applies per visit -- which is how
                // success/failure callback pairs are written, and flagging
                // those is noise that trains the reader to ignore this lint.
                // `B` with no `to` stays put and can interleave; a `to` back
                // into a state `A` fires from is the same hazard.
                let leaves_a_enabled = match other.to.as_deref() {
                    None => true,
                    Some(landing) => sources.contains(landing),
                };
                if !leaves_a_enabled || !guard_may_hold_after(action, other, &overlap) {
                    continue;
                }
                findings.push(LintFinding::warning(
                    "absorbing_effect",
                    format!(
                        "action '{}' assigns a literal to '{}' and enters '{}', where invariant \
                         '{}' reads '{}'; input action '{}' also writes '{}' and is legal in {:?}, \
                         so a concurrent observation can be discarded and '{}' then holds only \
                         because '{}' asserted it. Either derive '{}' from a param of '{}' so the \
                         value reports an observation, or forbid '{}' in {:?}, or target a state \
                         that claims nothing about '{}'",
                        action.name,
                        var,
                        target,
                        invariant.name,
                        var,
                        other.name,
                        var,
                        overlap,
                        invariant.name,
                        action.name,
                        var,
                        action.name,
                        other.name,
                        overlap,
                        var,
                    ),
                ));
            }
        }
    }
}

/// Suppress a collision only when known post-effect values prove the guard
/// false. Everything else stays unknown, so this remains a conservative lint.
fn guard_may_hold_after(action: &Action, other: &Action, overlap: &[&str]) -> bool {
    struct PostEffect<'a> {
        status: &'a str,
        booleans: BTreeMap<&'a str, bool>,
    }
    impl Env for PostEffect<'_> {
        fn status(&self) -> Val<'_> {
            Val::Str(self.status)
        }
        fn var(&self, name: &str) -> Val<'_> {
            self.booleans
                .get(name)
                .map_or(Val::Unknown, |value| Val::Bool(*value))
        }
        fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
            None
        }
    }
    let mut env = PostEffect {
        status: "",
        booleans: BTreeMap::new(),
    };
    for effect in &other.effect {
        if let Effect::Assign { var, op, value } = effect {
            // Effects are ordered. A later parameter-derived assignment
            // invalidates an earlier literal, rather than proving exclusion.
            env.booleans.remove(var.as_str());
            if let (AssignOp::Set, Arg::Lit(Literal::Bool(value))) = (op, value) {
                env.booleans.insert(var, *value);
            }
        }
    }
    overlap.iter().any(|source| {
        env.status = other.to.as_deref().unwrap_or(source);
        eval(&action.guard, &env).may_hold()
    })
}

/// The variables an action assigns a bare literal to with `=`.
///
/// `+=` and `-=` are excluded: they read the current value, so a concurrent
/// write is folded in rather than dropped.
fn literal_writes(action: &Action) -> Vec<&str> {
    action
        .effect
        .iter()
        .filter_map(|effect| match effect {
            Effect::Assign {
                var,
                op: AssignOp::Set,
                value: Arg::Lit(_),
            } => Some(var.as_str()),
            _ => None,
        })
        .collect()
}

/// Whether an action writes `var` by any means.
fn writes_var(action: &Action, var: &str) -> bool {
    action.effect.iter().any(|effect| match effect {
        Effect::Assign { var: written, .. } => written == var,
        Effect::Append { list, .. }
        | Effect::RemoveAt { list, .. }
        | Effect::Remove { list, .. } => list == var,
        _ => false,
    })
}

/// The states an action can fire from, expanding an omitted `from`.
///
/// An input or composite action with no `from` is enabled everywhere, which is
/// the I/O automata default the parser also applies; reading `from` literally
/// would miss every collision involving one.
fn effective_from<'a>(automaton: &'a Automaton, action: &'a Action) -> BTreeSet<&'a str> {
    if !action.from.is_empty() {
        return action.from.iter().map(String::as_str).collect();
    }
    if action.kind.enabled_everywhere() {
        return automaton
            .automaton
            .states
            .iter()
            .map(String::as_str)
            .collect();
    }
    BTreeSet::new()
}

/// Whether `assert` constrains `var` in `status`.
///
/// Handles the `status in [...] => ...` shape invariants are normally written
/// in by reading the statuses off the antecedent and the variables off the
/// consequent, and falls back to treating the whole expression as both.
fn polices_var_in_status(assert: &crate::predicate::Expr, status: &str, var: &str) -> bool {
    use crate::predicate::{Expr, Name};
    let (scope, body) = match assert {
        Expr::Implies(when, require) => (when.as_ref(), require.as_ref()),
        other => (other, other),
    };
    if !scope
        .required_statuses()
        .is_some_and(|statuses| statuses.contains(status))
    {
        return false;
    }
    let mut reads = false;
    body.for_each_name(&mut |name| {
        if let Name::Var(seen) = name
            && seen == var
        {
            reads = true;
        }
    });
    reads
}
