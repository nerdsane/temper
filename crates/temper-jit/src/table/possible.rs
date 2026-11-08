//! Whether a stored record can make a move, and why not.
//!
//! A new version of a spec is checked with these before it replaces the
//! installed one (see temper-server's `spec_change`): a stored record that
//! could move on under the installed version must still be able to under the
//! new one, or be carried to where it can. A move is possible when its rule
//! starts from the record's status and its guard holds on the record's
//! values; a guard that reads the caller's parameters is decided by its parts
//! that read none. A declared value the record lacks reads as its initial value, so a
//! parameter constraint can always read its field and depends only on the
//! caller's parameters.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use temper_spec::automaton::{ActionConstraint, is_carry_move};
use temper_spec::predicate::{Expr, Name};

use super::guard::{self, EvalContext, GuardFailure};
use super::types::TransitionTable;

/// Why a record cannot make a move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// The version has no rule for the action.
    NoAction,
    /// No rule for the action starts from the record's status.
    NotFrom,
    /// The guard is false; its failing part, when one is named.
    Guard(Option<GuardFailure>),
}

/// Where a stored record stands under a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Some move callers can make is possible for it.
    Free,
    /// No move starts from its state: it rests there.
    Rests,
    /// Moves start from its state and none is possible for it: each move,
    /// with why not.
    Stuck(Vec<(String, Blocked)>),
    /// Its state is not one of the version's.
    Gone,
}

/// What decides whether a record can make one move: its rules in order, with
/// the states each starts from and its guard, its constraints, and how the
/// names they read are declared. Two versions with equal conditions for a
/// move allow it for the same records.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveConditions {
    rules: Vec<(BTreeSet<String>, Expr)>,
    constraints: Vec<ActionConstraint>,
    declared: BTreeMap<String, Declared>,
}

/// How a version declares a name a move reads, with its initial value.
#[derive(Debug, Clone, PartialEq)]
enum Declared {
    Counter(usize),
    Bool(bool),
    List(Vec<String>),
    Field(Value),
    Undeclared,
}

impl TransitionTable {
    /// Why a record in `status` with the values in `ctx` cannot make
    /// `action`, or `None` when it can, for some call when a guard reads the
    /// caller's parameters.
    pub fn blocked(&self, status: &str, ctx: &EvalContext, action: &str) -> Option<Blocked> {
        let Some(result) = self.evaluate_ctx(status, ctx, action) else {
            return Some(Blocked::NoAction);
        };
        if result.success || self.holds_for_some_call(status, ctx, action) {
            return None;
        }
        Some(if self.starts_from(action, status) {
            Blocked::Guard(result.guard_failure)
        } else {
            Blocked::NotFrom
        })
    }

    /// Whether a rule for `action` from `status` has a guard that reads the
    /// caller's parameters and whose parts that read none hold: the record
    /// alone cannot say that no call makes the move.
    fn holds_for_some_call(&self, status: &str, ctx: &EvalContext, action: &str) -> bool {
        self.rules
            .iter()
            .filter(|rule| {
                rule.name == action
                    && (rule.from_states.is_empty() || rule.from_states.iter().any(|s| s == status))
            })
            .any(|rule| {
                reads_params(&rule.guard)
                    && guard::check_detailed(
                        &without_params(&rule.guard, true),
                        status,
                        ctx,
                        &self.initial_values,
                    )
                    .is_none()
            })
    }

    /// Where a record in `status` with the values in `ctx` stands. A
    /// `[[carry]]` move is the platform's, not a way forward.
    pub fn standing(&self, status: &str, ctx: &EvalContext) -> Standing {
        if !self.states.iter().any(|state| state == status) {
            return Standing::Gone;
        }
        let mut blocked = Vec::new();
        for action in self.caller_moves() {
            if !self.starts_from(action, status) {
                continue;
            }
            match self.blocked(status, ctx, action) {
                None => return Standing::Free,
                Some(why) => blocked.push((action.to_string(), why)),
            }
        }
        if blocked.is_empty() {
            Standing::Rests
        } else {
            Standing::Stuck(blocked)
        }
    }

    /// The moves callers can make, once each, in rule order.
    pub fn caller_moves(&self) -> Vec<&str> {
        let mut moves: Vec<&str> = Vec::new();
        for rule in &self.rules {
            if !is_carry_move(&rule.name) && !moves.contains(&rule.name.as_str()) {
                moves.push(&rule.name);
            }
        }
        moves
    }

    /// Whether a rule for `action` starts from `status`.
    pub fn starts_from(&self, action: &str, status: &str) -> bool {
        self.rules.iter().any(|rule| {
            rule.name == action
                && (rule.from_states.is_empty() || rule.from_states.iter().any(|s| s == status))
        })
    }

    /// The names `action`'s guards and constraints read from the record.
    pub fn move_reads(&self, action: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for rule in self.rules.iter().filter(|rule| rule.name == action) {
            rule.guard.for_each_name(&mut |name| {
                if let Name::Var(var) = name {
                    names.insert(var.to_string());
                }
            });
        }
        if let Some(contract) = self.action_contracts.get(action) {
            names.extend(
                contract
                    .constraints
                    .iter()
                    .filter_map(ActionConstraint::field)
                    .map(str::to_string),
            );
        }
        names
    }

    /// What decides whether a record can make `action` under this version.
    pub fn move_conditions(&self, action: &str) -> MoveConditions {
        let declared = &self.initial_values;
        MoveConditions {
            rules: self
                .rules
                .iter()
                .filter(|rule| rule.name == action)
                .map(|rule| {
                    (
                        rule.from_states.iter().cloned().collect(),
                        rule.guard.clone(),
                    )
                })
                .collect(),
            constraints: self
                .action_contracts
                .get(action)
                .map(|contract| contract.constraints.clone())
                .unwrap_or_default(),
            declared: self
                .move_reads(action)
                .into_iter()
                .map(|name| {
                    let how = if let Some(value) = declared.counters.get(&name) {
                        Declared::Counter(*value)
                    } else if let Some(value) = declared.booleans.get(&name) {
                        Declared::Bool(*value)
                    } else if let Some(value) = declared.lists.get(&name) {
                        Declared::List(value.clone())
                    } else if let Some(value) = declared.fields.get(&name) {
                        Declared::Field(value.clone())
                    } else {
                        Declared::Undeclared
                    };
                    (name, how)
                })
                .collect(),
        }
    }

    /// The context a stored record holding `values` gives this version's
    /// guards: each value under the kind this version declares its name, and
    /// as a field when it declares none or the value is not of that kind.
    pub fn stored_context(&self, values: &BTreeMap<String, Value>) -> EvalContext {
        let declared = &self.initial_values;
        let mut ctx = EvalContext::default();
        for (name, value) in values {
            if declared.counters.contains_key(name)
                && let Some(count) = value.as_u64()
            {
                ctx.counters.insert(name.clone(), count as usize);
            } else if declared.booleans.contains_key(name)
                && let Some(flag) = value.as_bool()
            {
                ctx.booleans.insert(name.clone(), flag);
            } else if declared.lists.contains_key(name)
                && let Some(items) = value.as_array().and_then(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                })
            {
                ctx.lists.insert(name.clone(), items);
            } else {
                ctx.fields.insert(name.clone(), value.clone());
            }
        }
        ctx
    }
}

/// Whether `expr` reads a parameter of the call.
fn reads_params(expr: &Expr) -> bool {
    let mut reads = false;
    expr.for_each_name(&mut |name| reads |= matches!(name, Name::Param(_)));
    reads
}

/// `guard` with each part that reads a parameter replaced by the value some
/// call could give it: `holds` where the part counts toward the guard
/// holding, its opposite under a `!` or left of a `=>`. It holds on a record
/// when some call could make `guard` hold, so the parts that read none still
/// decide, in whichever branch they sit.
fn without_params(guard: &Expr, holds: bool) -> Expr {
    if !reads_params(guard) {
        return guard.clone();
    }
    let each = |parts: &[Expr]| -> Vec<Expr> {
        parts
            .iter()
            .map(|part| without_params(part, holds))
            .collect()
    };
    match guard {
        Expr::Not(inner) => Expr::Not(Box::new(without_params(inner, !holds))),
        Expr::And(parts) => Expr::And(each(parts)),
        Expr::Or(parts) => Expr::Or(each(parts)),
        Expr::Implies(when, then) => Expr::Implies(
            Box::new(without_params(when, !holds)),
            Box::new(without_params(then, holds)),
        ),
        _ => Expr::Const(holds),
    }
}

#[cfg(test)]
#[path = "possible_test.rs"]
mod tests;
