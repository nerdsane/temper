//! Whether records will reach a condition, under fairness.
//!
//! `liveness.rs` checks that a way to the condition exists from every state.
//! That is the same as "it comes" only if every move is strongly fair, and it
//! cannot say which moves are. A rule with `fairness` also gets this check:
//! from every reachable state, every fair behaviour reaches the condition with
//! nothing in flight. In temporal terms, `[]<>Done` under the fairness below.
//!
//! The method is TLC's (`tlc2/tool/liveness`), read through its TypeScript
//! rewrite tla-ts (`liveness-checker.ts`, `tableau.ts`; both MIT): negate the
//! property, conjoin the fairness, and search the found graph's strongly
//! connected components for a cycle that satisfies the result. Stuttering is
//! allowed, so one state counts as a cycle. Three things follow from our rules
//! being `[]<>Done` and from the graph already being found:
//!
//! - The negation `<>[]~Done` only limits which states a cycle may hold, so no
//!   tableau is needed. A rule language with leads-to would need one.
//! - Weak fairness, `[]<>(~ENABLED a \/ a taken)`, is judged on a whole
//!   component: if it fails there it fails on every cycle inside.
//! - Strong fairness, `<>[]~ENABLED a \/ []<>(a taken)`, expands to two branches
//!   per constraint in TLC. Here a component where `a` is possible and never
//!   taken loses the states where it is possible and is searched again, which
//!   gives the same verdict without the 2^k branches.
//!
//! A move that only stays where it is does not count as taken or as possible:
//! TLC's `<<A>>_vars`. Two such moves are real events the joint state cannot
//! show, so they count as taken: a call that ended and started again, and a
//! `reset_on` move.
//!
//! # What is fair by default
//!
//! Each default is what the kernel does, in `temper-server`:
//!
//! - A state timer, weakly, one for each state even when two fire one action.
//!   Its row is durable and is armed again at boot
//!   (`state/dispatch/state_timeouts.rs`). It fires once while its record stays
//!   in the state; leaving cancels it, so nothing owes it when the record keeps
//!   leaving, and a `reset_on` move starts its clock again, so nothing owes it
//!   while those keep coming. Strong would be false.
//! - A module call ending, weakly, by any answer, its failure or no answer at
//!   all as one move. The engine stops a call at its deadline
//!   (`state/dispatch/call_deadlines.rs`) and a call that fails is answered once.
//!   The kernel owes the ending, never the success, so a call that fails and is
//!   retried forever is a fair cycle, and one that ends with no answer leaves its
//!   record where it is unless something else moves it.
//!   A rule that means "it succeeds in the end" says `strong = ["Answer"]`.
//! - A dispatch's delivery, weakly. A reaction is attempted after its source
//!   commits (`trigger/dispatcher.rs`), and only a store failure retries it
//!   (`state/dispatch/owed_writes.rs`). A refusal is final; the walk has that
//!   as the move's refused way, so it counts as taken.
//! - An action in `relies_on`, weakly and only while the condition is false:
//!   the rule's own statement of what code outside the specs does.
//!
//! Not fair: a request from outside, which nothing makes come; a call lost to a
//! restart; and a reaction from a type outside the scope, which nothing the
//! model holds says comes.
//!
//! Two things the kernel can do have no move in the walk, so HOLDS assumes
//! they do not happen: a restart loses a reaction in flight
//! (`OwedKind::Reaction`, "a restart loses it"), and a full mailbox refuses one
//! for good (`a_full_mailbox_a_refusal_and_a_conflict_are_final`).
//!
//! # What a verdict rests on
//!
//! HOLDS and BROKEN each search with what they may count on, as a stuck verdict
//! does. HOLDS: no cycle even when every way the model cannot decide is taken,
//! and a move constrains a cycle only if it is sure to be possible. BROKEN: a
//! cycle that sure moves alone reach and run, which stays fair even when every
//! move the model cannot rule out is possible. Anything between is not checked.
//!
//! A rule's condition is about the system left alone, so a cycle is first
//! searched without requests from outside, other than those the rule names. If
//! none is found the rule holds, and a cycle that needs someone outside to keep
//! acting forever is reported beside it: TLC, with those requests in `Next`,
//! would call it a violation.

use std::fmt;

use temper_spec::automaton::Fairness;
use temper_spec::predicate::Expr;

use super::JointStep;
use super::liveness::{Marks, marks};
use super::model::{Explored, JointModel};

#[path = "fair/graph.rs"]
mod graph;
#[path = "fair/lasso.rs"]
mod lasso;
#[path = "fair/search.rs"]
mod search;

use graph::Graph;
use lasso::{lasso, unsure_reason};
use search::{Rules, Side, View};

/// What the fairness check found for one rule.
#[derive(Debug, Clone)]
pub struct FairVerdict {
    /// Actions the rule assumes weakly fair, beyond what the runtime owes.
    pub weak: Vec<String>,
    /// Actions the rule assumes strongly fair.
    pub strong: Vec<String>,
    /// Actions the rule assumes come while its condition is false. The walk
    /// takes the rule's word for them, so the verdict names them.
    pub relies_on: Vec<String>,
    /// When the walk held a move back, the bound on what was in flight.
    pub in_flight_bound: Option<usize>,
    /// Holds, broken, proves nothing, or not fully checked.
    pub outcome: FairOutcome,
}

/// The outcome of the fairness check.
#[derive(Debug, Clone)]
pub enum FairOutcome {
    /// Every fair behaviour of the system left alone reaches the condition.
    Holds {
        /// A fair cycle that never reaches it, when one needs someone outside
        /// to keep acting forever.
        kept_away: Option<Lasso>,
        /// With none sure, why one may still be there: moves the model cannot
        /// decide (what Cedar says, a guard) may make such a cycle.
        may_be_kept_away: Option<String>,
    },
    /// A fair behaviour never reaches it.
    Broken {
        /// That behaviour.
        lasso: Lasso,
    },
    /// No fair cycle misses the condition, because the walk found nothing that
    /// could: no state meets its premise, or every state already meets it.
    Vacuous {
        /// Which, in words.
        why: String,
    },
    /// The walk stopped, or the verdict would rest on what the model cannot
    /// decide.
    Incomplete {
        /// Why.
        reason: String,
    },
}

/// A behaviour that goes on forever: a path, then a cycle back to its end.
#[derive(Debug, Clone)]
pub struct Lasso {
    /// From a start to the state the cycle leaves from and comes back to.
    pub prefix: Vec<JointStep>,
    /// The moves around the cycle, ending where it began. Empty when the
    /// behaviour simply stays there.
    pub cycle: Vec<JointStep>,
}

impl fmt::Display for Lasso {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.prefix {
            match &step.action {
                Some(action) => writeln!(f, "    {action}\n        {}", step.state)?,
                None => writeln!(f, "    start\n        {}", step.state)?,
            }
        }
        if self.cycle.is_empty() {
            return writeln!(f, "  and stays there: nothing owed moves it");
        }
        writeln!(f, "  then forever:")?;
        for step in &self.cycle {
            let action = step.action.as_deref().unwrap_or_default();
            writeln!(f, "    {action}\n        {}", step.state)?;
        }
        Ok(())
    }
}

impl fmt::Display for FairVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut under = String::from("the runtime's fairness");
        if !self.weak.is_empty() {
            under += &format!(", weak on {}", self.weak.join(", "));
        }
        if !self.strong.is_empty() {
            under += &format!(", strong on {}", self.strong.join(", "));
        }
        if !self.relies_on.is_empty() {
            under += &format!(
                ", relying on {} while it is false",
                self.relies_on.join(", ")
            );
        }
        if let Some(bound) = self.in_flight_bound {
            under += &format!(", with at most {bound} dispatches and calls in flight");
        }
        match &self.outcome {
            FairOutcome::Holds {
                kept_away: None,
                may_be_kept_away: None,
            } => write!(f, "HOLDS under {under}"),
            FairOutcome::Holds {
                kept_away: None,
                may_be_kept_away: Some(why),
            } => write!(
                f,
                "HOLDS under {under} when left alone. With moves nothing owes (requests, restarts) going on forever, {why}"
            ),
            FairOutcome::Holds {
                kept_away: Some(lasso),
                ..
            } => write!(
                f,
                "HOLDS under {under} when left alone, though moves nothing owes (requests, restarts) can keep it away if they go on forever:\n{lasso}"
            ),
            FairOutcome::Broken { lasso } => write!(
                f,
                "BROKEN under {under}: a fair behaviour never reaches it:\n{lasso}"
            ),
            FairOutcome::Vacuous { why } => {
                write!(f, "CHECKS NOTHING AS WRITTEN under {under}: {why}")
            }
            FairOutcome::Incomplete { reason } => {
                write!(f, "NOT CHECKED under {under}: {reason}")
            }
        }
    }
}

/// Check a rule's condition under fairness over a finished walk.
pub(super) fn fair_check(
    model: &JointModel,
    owner: &str,
    goal: &Expr,
    explored: &Explored,
    fairness: &Fairness,
) -> FairOutcome {
    let Marks {
        done,
        failing,
        maybe_done,
        met,
    } = marks(model, owner, goal, explored);
    let graph = match Graph::new(model, explored) {
        Ok(graph) => graph,
        Err(reason) => return FairOutcome::Incomplete { reason },
    };
    let rules = Rules::new(&graph, owner, fairness);
    let may_open: Vec<bool> = done.iter().map(|&done| !done).collect();
    let view = |side, people, open| View {
        graph: &graph,
        rules: &rules,
        failing: &failing,
        side,
        people,
        open,
    };
    let may = view(Side::May, false, &may_open);
    let unsure = may.fair_cycle();
    // BROKEN and `kept_away` rest on what the runtime can do: sure moves alone
    // reach the cycle, and the condition may not hold anywhere on it.
    let parents = explored.sure_parents();
    let must_open: Vec<bool> = maybe_done
        .iter()
        .zip(&parents)
        .map(|(&maybe, parent)| !maybe && parent.is_some())
        .collect();
    if let Some(unsure) = unsure {
        let must = view(Side::Must, false, &must_open);
        return match must.fair_cycle() {
            Some(cycle) => FairOutcome::Broken {
                lasso: lasso(model, explored, &must, &parents, &cycle),
            },
            None => FairOutcome::Incomplete {
                reason: unsure_reason(explored, &may, &unsure),
            },
        };
    }
    if !met {
        let premise = super::premise(goal)
            .map(ToString::to_string)
            .unwrap_or_default();
        return FairOutcome::Vacuous {
            why: format!("its premise `{premise}` holds in no state"),
        };
    }
    // A record the walk never saw miss the condition proves nothing about what
    // happens when one does.
    if done.iter().all(|&done| done) {
        return FairOutcome::Vacuous {
            why: format!("`{goal}` is already met in every state the walk found"),
        };
    }
    let people = view(Side::Must, true, &must_open);
    let kept_away = people
        .fair_cycle()
        .map(|cycle| lasso(model, explored, &people, &parents, &cycle));
    let may_be_kept_away = if kept_away.is_some() {
        None
    } else {
        let people = view(Side::May, true, &may_open);
        people
            .fair_cycle()
            .map(|cycle| unsure_reason(explored, &people, &cycle))
    };
    FairOutcome::Holds {
        kept_away,
        may_be_kept_away,
    }
}

/// The bytes [`fair_check`] holds for `explored`, at its peak: the marks and
/// sure parents; the graph's lists; one view's next-state lists; and one
/// search's marks, stack and component lists. The table of distinct moves grows
/// with the app's actions, not with the walk, so it is not counted.
pub(super) fn fair_bytes(explored: &Explored<'_>) -> usize {
    let states = explored.len();
    let (moves, edges) = (explored.made.len(), explored.edges.len());
    let per_state = 3 * size_of::<bool>()
        + 2 * size_of::<bool>()
        + 2 * size_of::<Option<u32>>()
        + 2 * size_of::<u32>()
        + 7 * size_of::<u32>()
        + size_of::<(u32, u32)>();
    let per_move = 3 * size_of::<u32>();
    let per_edge = 2 * size_of::<u32>();
    states * per_state + moves * per_move + edges * per_edge
}

#[cfg(test)]
#[path = "fair_test.rs"]
mod tests;
