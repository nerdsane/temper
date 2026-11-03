//! Symbolic state and predicate encoding shared by the L0 checks.
use crate::model::TemperModel;
use crate::model::semantics::{collect_list_contains_pairs, counter_bound, list_bound};
use std::collections::{BTreeMap, BTreeSet};
use temper_spec::predicate::{CmpOp, Expr, Literal, Operand, Set};
use z3::Solver;
use z3::ast::{Bool, Int};

// ---------------------------------------------------------------------------
// Z3 helpers
// ---------------------------------------------------------------------------

/// Create Z3 integer variables for each counter, each in `[0, bound]` with
/// the bound L1 explores it to, which covers every literal it is compared with.
pub(super) fn make_counter_vars(model: &TemperModel, solver: &Solver) -> Vec<(String, Int)> {
    let zero = Int::from_i64(0);
    model
        .initial_counters
        .keys()
        .map(|name| {
            let var = Int::new_const(var_name("", "counter", name));
            let bound = i64::try_from(counter_bound(model, name)).unwrap_or(i64::MAX);
            solver.assert(var.ge(&zero));
            solver.assert(var.le(Int::from_i64(bound)));
            (name.clone(), var)
        })
        .collect()
}

/// Create Z3 boolean variables for each boolean state var.
pub(super) fn make_bool_vars(model: &TemperModel) -> Vec<(String, Bool)> {
    model
        .initial_booleans
        .keys()
        .map(|name| {
            let var = Bool::new_const(var_name("", "bool", name));
            (name.clone(), var)
        })
        .collect()
}

#[derive(Clone, Default)]
pub(super) struct ListSymbolicVars {
    pub(super) len_vars: BTreeMap<String, Int>,
    /// Whether each list holds each value a guard or invariant looks for in
    /// it.
    pub(super) holds: BTreeMap<String, BTreeMap<String, Bool>>,
}

/// Create exact bounded symbolic list variables, named after `prefix`, for
/// each list up to the length L1 explores it to (its bound):
/// - its length, in `[0, bound]`
/// - whether it holds each value a guard or invariant looks for in it, with
///   no more of those held than its length
///
/// Membership is all a guard or invariant reads of a list's elements, and a
/// list of length `n` can hold any `n` of the values looked for, so this is
/// exact. A constant per position would grow with the bound, which comes
/// from the literals a spec compares the length with and can be any size.
pub(super) fn make_list_vars(
    model: &TemperModel,
    solver: &Solver,
    prefix: &str,
) -> ListSymbolicVars {
    let zero = Int::from_i64(0);
    let one = Int::from_i64(1);
    let mut looked_for: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let exprs = model
        .transitions
        .iter()
        .map(|t| &t.guard)
        .chain(model.invariants.iter().map(|inv| &inv.assert));
    for expr in exprs {
        let mut pairs = BTreeSet::new();
        collect_list_contains_pairs(expr, &mut pairs);
        for (list, value) in pairs {
            looked_for.entry(list).or_default().insert(value);
        }
    }

    let mut len_vars = BTreeMap::new();
    let mut holds = BTreeMap::new();
    for name in model.initial_lists.keys() {
        let bound = list_bound(model, name);
        let max_val = Int::from_i64(i64::try_from(bound).unwrap_or(i64::MAX));
        let len_var = Int::new_const(var_name(prefix, "len", name));
        solver.assert(len_var.ge(&zero));
        solver.assert(len_var.le(&max_val));

        let held: BTreeMap<String, Bool> = looked_for
            .get(name)
            .into_iter()
            .flatten()
            .map(|value| {
                let constant =
                    Bool::new_const(var_name(prefix, "holds", &format!("{name}|{value}")));
                (value.clone(), constant)
            })
            .collect();
        if !held.is_empty() {
            let counted: Vec<Int> = held.values().map(|h| h.ite(&one, &zero)).collect();
            let count = Int::add(&counted.iter().collect::<Vec<_>>());
            solver.assert(count.le(&len_var));
        }
        len_vars.insert(name.clone(), len_var);
        holds.insert(name.clone(), held);
    }

    ListSymbolicVars { len_vars, holds }
}

/// A Z3 constant's name: the state it belongs to (`prefix`), the kind of
/// value, and the spec's own name last, so constants of different kinds never
/// share a name (a counter `tags_len` and list `tags`'s length are two
/// constants).
pub(super) fn var_name(prefix: &str, kind: &str, name: &str) -> String {
    format!("{prefix}{kind}|{name}")
}

/// Create a symbolic status variable over `model.states` indices.
pub(super) fn make_status_var(model: &TemperModel, solver: &Solver) -> Int {
    let var = Int::new_const("status");
    let zero = Int::from_i64(0);
    if model.states.is_empty() {
        solver.assert(var.eq(&zero));
        return var;
    }
    let max = Int::from_i64((model.states.len() - 1) as i64);
    solver.assert(var.ge(&zero));
    solver.assert(var.le(&max));
    var
}

/// Encode `status ∈ states` as a disjunction over symbolic status index.
pub(super) fn encode_state_membership(
    status_var: &Int,
    states: &[String],
    model: &TemperModel,
) -> Bool {
    let disjuncts: Vec<Bool> = states
        .iter()
        .filter_map(|state| {
            model
                .states
                .iter()
                .position(|s| s == state)
                .map(|idx| status_var.eq(Int::from_i64(idx as i64)))
        })
        .collect();
    if disjuncts.is_empty() {
        Bool::from_bool(false)
    } else {
        Bool::or(&disjuncts)
    }
}

/// Encode exact bounded `contains(list, value)`: whether the list holds it.
pub(super) fn encode_list_contains(var: &str, value: &str, lists: &ListSymbolicVars) -> Bool {
    lists
        .holds
        .get(var)
        .and_then(|held| held.get(value))
        .cloned()
        .unwrap_or_else(|| Bool::from_bool(false))
}

/// Symbolic values for one state.
pub(super) struct Symbols<'a> {
    pub(super) counters: BTreeMap<String, Int>,
    pub(super) bools: BTreeMap<String, Bool>,
    pub(super) strings: BTreeMap<String, Int>,
    pub(super) lists: &'a ListSymbolicVars,
    pub(super) status: Int,
    /// Names this state's free atoms (pre/post) in a solver dump.
    pub(super) prefix: &'static str,
}

/// Encode an expression as a Z3 formula. Each part over values the model
/// does not track (related-entity statuses, fields, parameters) is a
/// fresh free atom, even where the same text appears twice: L1 reads every
/// such part as unknown, so `X && !X` may hold there, and L0 must not rule
/// out what L1 explores.
pub(super) fn encode(expr: &Expr, sym: &Symbols<'_>, model: &TemperModel) -> Bool {
    let free = || Bool::fresh_const(&format!("{}unknown", sym.prefix));
    match expr {
        Expr::Const(value) => Bool::from_bool(*value),
        Expr::Not(inner) => encode(inner, sym, model).not(),
        Expr::And(parts) => Bool::and(
            &parts
                .iter()
                .map(|p| encode(p, sym, model))
                .collect::<Vec<_>>(),
        ),
        Expr::Or(parts) => Bool::or(
            &parts
                .iter()
                .map(|p| encode(p, sym, model))
                .collect::<Vec<_>>(),
        ),
        Expr::Implies(lhs, rhs) => encode(lhs, sym, model).implies(encode(rhs, sym, model)),
        Expr::Var(name) => sym.bools.get(name).cloned().unwrap_or_else(free),
        Expr::Empty(name) => match sym.lists.len_vars.get(name) {
            Some(len) => len.eq(Int::from_i64(0)),
            None => sym
                .strings
                .get(name)
                .map_or_else(free, |value| value.eq(Int::from_i64(0))),
        },
        Expr::Compare { lhs, op, rhs } => {
            if let (Some(a), Some(b)) = (int_term(lhs, sym), int_term(rhs, sym)) {
                return match op {
                    CmpOp::Eq => a.eq(&b),
                    CmpOp::Ne => a.eq(&b).not(),
                    CmpOp::Lt => a.lt(&b),
                    CmpOp::Le => a.le(&b),
                    CmpOp::Gt => a.gt(&b),
                    CmpOp::Ge => a.ge(&b),
                };
            }
            if let (Some(a), Some(b)) = (string_term(lhs, sym, model), string_term(rhs, sym, model))
            {
                return match op {
                    CmpOp::Eq => a.eq(&b),
                    CmpOp::Ne => a.eq(&b).not(),
                    _ => free(),
                };
            }
            let equal = match (lhs, rhs) {
                (Operand::Status, Operand::Lit(Literal::Str(state)))
                | (Operand::Lit(Literal::Str(state)), Operand::Status) => Some(
                    encode_state_membership(&sym.status, std::slice::from_ref(state), model),
                ),
                (Operand::Var(name), Operand::Lit(Literal::Bool(value)))
                | (Operand::Lit(Literal::Bool(value)), Operand::Var(name)) => sym
                    .bools
                    .get(name)
                    .map(|var| var.eq(Bool::from_bool(*value))),
                _ => None,
            };
            match (equal, op) {
                (Some(equal), CmpOp::Eq) => equal,
                (Some(equal), CmpOp::Ne) => equal.not(),
                _ => free(),
            }
        }
        Expr::In {
            value,
            set,
            negated,
        } => {
            let member = match (value, set) {
                (value, Set::List(items))
                    if string_term(value, sym, model).is_some()
                        && !matches!(value, Operand::Status) =>
                {
                    let term = string_term(value, sym, model).expect("checked above");
                    let options: Option<Vec<Bool>> = items
                        .iter()
                        .map(|item| match item {
                            Literal::Str(value) => {
                                string_literal(value, model).map(|value| term.eq(value))
                            }
                            _ => None,
                        })
                        .collect();
                    options.map(|options| Bool::or(&options))
                }
                (Operand::Status, Set::List(items)) => {
                    let states: Option<Vec<String>> = items
                        .iter()
                        .map(|item| match item {
                            Literal::Str(state) => Some(state.clone()),
                            _ => None,
                        })
                        .collect();
                    states.map(|states| encode_state_membership(&sym.status, &states, model))
                }
                (Operand::Lit(Literal::Str(value)), Set::Var(list))
                    if sym.lists.len_vars.contains_key(list) =>
                {
                    Some(encode_list_contains(list, value, sym.lists))
                }
                (value, Set::List(items)) => int_term(value, sym).and_then(|term| {
                    let options: Option<Vec<Bool>> = items
                        .iter()
                        .map(|item| match item {
                            Literal::Int(n) => Some(term.eq(Int::from_i64(*n))),
                            _ => None,
                        })
                        .collect();
                    options.map(|options| Bool::or(&options))
                }),
                _ => None,
            };
            match member {
                Some(member) if *negated => member.not(),
                Some(member) => member,
                None => free(),
            }
        }
    }
}

/// An integer-valued operand: a counter, a list length, or an integer.
pub(super) fn int_term(operand: &Operand, sym: &Symbols<'_>) -> Option<Int> {
    match operand {
        Operand::Var(name) => sym.counters.get(name).cloned(),
        Operand::Len(name) => sym.lists.len_vars.get(name).cloned(),
        Operand::Lit(Literal::Int(n)) => Some(Int::from_i64(*n)),
        _ => None,
    }
}

/// Equality atoms for tracked strings. Zero is the empty string; each
/// concrete spec literal has its own positive atom. Parameter-written fields
/// remain unconstrained, so values outside those literals are still covered.
pub(super) fn make_string_vars(model: &TemperModel, prefix: &str) -> BTreeMap<String, Int> {
    model
        .tracked_strings
        .iter()
        .map(|name| {
            (
                name.clone(),
                Int::new_const(format!("{prefix}string:{name}")),
            )
        })
        .collect()
}

pub(super) fn string_literal(value: &str, model: &TemperModel) -> Option<Int> {
    if value.is_empty() {
        return Some(Int::from_i64(0));
    }
    let literals: BTreeSet<&str> = model
        .string_literals
        .values()
        .flat_map(|values| values.iter().map(String::as_str))
        .filter(|value| !value.is_empty())
        .collect();
    literals
        .iter()
        .position(|literal| *literal == value)
        .map(|idx| Int::from_i64(idx as i64 + 1))
}

pub(super) fn string_term(
    operand: &Operand,
    sym: &Symbols<'_>,
    model: &TemperModel,
) -> Option<Int> {
    match operand {
        Operand::Var(name) => sym.strings.get(name).cloned(),
        Operand::Lit(Literal::Str(value)) => string_literal(value, model),
        _ => None,
    }
}

/// L0's existing predicate subset plus tracked string equality. Other string
/// operations and related fields remain the responsibility of L1/joint checks.
pub(super) fn models(expr: &Expr, model: &TemperModel) -> bool {
    use temper_spec::predicate::unmodelable;
    if unmodelable(expr, &model.var_kinds).is_none() {
        return true;
    }
    let string = |operand: &Operand| match operand {
        Operand::Var(name) => model.tracked_strings.contains(name),
        Operand::Lit(Literal::Str(_)) => true,
        _ => false,
    };
    match expr {
        Expr::Not(inner) => models(inner, model),
        Expr::And(parts) | Expr::Or(parts) => parts.iter().all(|part| models(part, model)),
        Expr::Implies(a, b) => models(a, model) && models(b, model),
        Expr::Empty(name) => model.tracked_strings.contains(name),
        Expr::Compare {
            lhs,
            op: CmpOp::Eq | CmpOp::Ne,
            rhs,
        } => string(lhs) && string(rhs),
        Expr::In {
            value,
            set: Set::List(items),
            ..
        } => string(value) && items.iter().all(|item| matches!(item, Literal::Str(_))),
        _ => false,
    }
}
