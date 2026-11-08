//! How far each count parameter of a move is explored: as far as the move's
//! own arithmetic keeps every counter the parameter lands in inside its
//! bound, and every list position it gives no further than one past the end.
//!
//! Each counter's value at the end of the move is written as the largest of
//! a few linear forms over the move's count parameters and what the counters
//! held before the move, each plus a constant part. A subtraction stops at
//! 0, which adds the form 0, so a counter ends inside its bound exactly when
//! each of its forms does. What a counter held before the move is known only
//! to lie between 0 and its bound, so each limit turns those terms into a
//! range, and the limits give each parameter the largest value it can take.

mod forms;

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{Arg, Literal, ParamKind};

use self::forms::{Coefficients, Rest, Terms, Value, fold_before};
use super::semantics::{counter_bound, list_bound};
use super::types::{ModelEffect, ResolvedTransition, TemperModel};

/// The values one count parameter of a move is explored over: `0..=largest`.
pub(super) struct CountRange {
    /// The largest value tried.
    pub(super) largest: usize,
    /// The counters, each with its bound, that a value past `largest` can
    /// take past that bound. Such a step is left out, so a pass over the
    /// move is bounded. Empty when every larger value ends the move as one
    /// tried does.
    pub(super) past: BTreeMap<String, usize>,
    /// Whether the range rests on a value the move's arithmetic does not
    /// settle: another parameter taken as 0 because nothing limits it, or a
    /// counter whose value could not be worked out. A pass over the move is
    /// then bounded.
    pub(super) widened: bool,
}

/// One limit on the move's parameters: `terms + rest <= bound`.
struct Limit<'a> {
    /// The counter whose bound it is, or `None` for a list position, where
    /// `bound` is the first position past the end, which acts as every later
    /// one does.
    counter: Option<&'a str>,
    terms: Coefficients<'a>,
    rest: Rest,
    bound: i64,
}

impl Limit<'_> {
    fn coefficient(&self, p: &str) -> i64 {
        self.terms.get(p).copied().unwrap_or(0)
    }
}

/// What the move's effects do with its count parameters.
struct Walked<'a> {
    /// The limits the bounds of the counters it writes, and the lists it
    /// removes from, put on them.
    limits: Vec<Limit<'a>>,
    /// Parameters that reach a counter or position whose value could not be
    /// worked out, and so lost the limit it puts on them.
    unsure: BTreeSet<&'a str>,
}

fn walk<'a>(model: &TemperModel, t: &'a ResolvedTransition) -> Walked<'a> {
    let mut values: BTreeMap<&str, Value> = BTreeMap::new();
    let mut limits = Vec::new();
    let mut unsure = BTreeSet::new();
    for (i, effect) in t.effects.iter().enumerate() {
        match effect {
            ModelEffect::SetCounter { var, value } => {
                let value = arg_value(&values, value);
                values.insert(var, value);
            }
            ModelEffect::AddCounter { var, value } => {
                let sum = value_of(&values, var).combine(&arg_value(&values, value), 1, model);
                values.insert(var, sum);
            }
            ModelEffect::SubCounter { var, value } => {
                let left = value_of(&values, var).take(&arg_value(&values, value), model);
                values.insert(var, left);
            }
            ModelEffect::ListRemoveAt { var, index } => {
                let appended = t.effects[..i]
                    .iter()
                    .filter(|earlier| {
                        matches!(earlier, ModelEffect::ListAppend { var: v, .. } if v == var)
                    })
                    .count();
                let past_end = whole(list_bound(model, var).saturating_add(appended));
                match arg_value(&values, index) {
                    Value::Largest(forms) => {
                        limits.extend(limits_of(model, &forms, None, past_end))
                    }
                    Value::Unknown(params) => unsure.extend(params),
                }
            }
            ModelEffect::SetBool { .. }
            | ModelEffect::SetString { .. }
            | ModelEffect::ListAppend { .. }
            | ModelEffect::ListRemove { .. } => {}
        }
    }
    for (counter, value) in values {
        match value {
            Value::Largest(forms) => {
                let bound = whole(counter_bound(model, counter));
                limits.extend(limits_of(model, &forms, Some(counter), bound));
            }
            Value::Unknown(params) => unsure.extend(params),
        }
    }
    Walked { limits, unsure }
}

/// The limits `value <= bound` puts on the move's parameters, one for each
/// sum of parameters in `forms`.
fn limits_of<'a>(
    model: &TemperModel,
    forms: &BTreeMap<Terms<'a>, Rest>,
    counter: Option<&'a str>,
    bound: i64,
) -> impl Iterator<Item = Limit<'a>> {
    fold_before(model, forms)
        .into_iter()
        .filter(|(terms, _)| !terms.params.is_empty())
        .map(move |(terms, rest)| Limit {
            counter,
            terms: terms.params,
            rest,
            bound,
        })
}

/// `var`'s value at this point of the move: what an earlier effect wrote,
/// or what it held before the move.
fn value_of<'a>(values: &BTreeMap<&'a str, Value<'a>>, var: &'a str) -> Value<'a> {
    values
        .get(var)
        .cloned()
        .unwrap_or_else(|| Value::before(var))
}

fn arg_value<'a>(values: &BTreeMap<&'a str, Value<'a>>, arg: &'a Arg) -> Value<'a> {
    match arg {
        // A negative amount counts as none, as in L1.
        Arg::Lit(Literal::Int(n)) => Value::rest((*n).max(0), (*n).max(0)),
        Arg::Param(p) => Value::param(p),
        Arg::Var(source) => value_of(values, source),
        Arg::Lit(_) => Value::rest(0, 0),
    }
}

/// The values worth trying for each count parameter of `t`.
///
/// A parameter goes as far as every counter it lands in stays inside its
/// bound. Where that depends on another parameter that nothing limits, as
/// `a` does in `x = a - b`, the other is taken as 0 and the range is
/// widened. A parameter nothing limits goes as far as it still changes how
/// the move ends: until a position it gives is past the list's end, and
/// until every form it lowers is at 0.
pub(super) fn count_ranges<'a>(
    model: &TemperModel,
    t: &'a ResolvedTransition,
) -> BTreeMap<&'a str, CountRange> {
    let counts: Vec<&str> = t
        .params
        .iter()
        .filter(|(_, kind)| matches!(kind, ParamKind::Count))
        .map(|(name, _)| name.as_str())
        .collect();
    if counts.is_empty() {
        return BTreeMap::new();
    }
    let walked = walk(model, t);
    let bounds: Vec<&Limit> = walked
        .limits
        .iter()
        .filter(|l| l.counter.is_some())
        .collect();
    let mut most: BTreeMap<&str, Option<i64>> = counts.iter().map(|p| (*p, None)).collect();
    tighten(&bounds, &mut most, None);
    let guessed: BTreeSet<&str> = counts
        .iter()
        .copied()
        .filter(|p| most[p].is_none() && bounds.iter().any(|l| l.coefficient(p) > 0))
        .collect();
    tighten(&bounds, &mut most, Some(&guessed));

    let widest = widest(model);
    let mut ranges: BTreeMap<_, _> = counts
        .iter()
        .map(|p| {
            let range = match most[p] {
                Some(limit) => CountRange {
                    largest: usize::try_from(limit).unwrap_or(0),
                    past: bounds
                        .iter()
                        .filter(|l| l.coefficient(p) > 0)
                        .filter_map(|l| l.counter)
                        .map(|counter| (counter.to_string(), counter_bound(model, counter)))
                        .collect(),
                    widened: guessed.contains(p),
                },
                None => {
                    let (reach, reach_guessed) = reach(&walked.limits, &most, p);
                    let worth = lowering_worth(&walked.limits, &most, p, widest).max(reach);
                    let unsure = walked.unsure.contains(p);
                    let largest = worth.unwrap_or(model.default_max_counter);
                    CountRange {
                        largest: if unsure { largest.max(widest) } else { largest },
                        past: BTreeMap::new(),
                        widened: unsure || reach_guessed,
                    }
                }
            };
            (*p, range)
        })
        .collect();
    if super::parameters::checks_inputs(model, t) {
        // Freeze effect ranges before widening: a compared input's effects
        // can require values this input must reach too. Updating in place
        // while calculating bounds would make the result depend on order.
        let guard_bounds: Vec<_> = t
            .guard
            .params_read()
            .into_iter()
            .filter(|p| ranges.contains_key(p.as_str()))
            .map(|p| {
                let bound = super::parameters::guard_count_bound(model, &t.guard, &p, &ranges);
                (p, bound)
            })
            .collect();
        for (p, bound) in guard_bounds {
            let range = ranges.get_mut(p.as_str()).expect("count input range");
            range.largest = range.largest.max(bound);
        }
    }
    ranges
}

/// Lower each parameter's largest value to what every bound allows, given
/// the others' largest values. A parameter nothing limits yet stops a bound
/// that subtracts it from saying anything, unless `guessing` names the
/// parameters to update taking it as 0. Each pass only lowers values, and
/// stopping early leaves them higher, which explores more, never less.
fn tighten<'a>(
    bounds: &[&Limit<'a>],
    most: &mut BTreeMap<&'a str, Option<i64>>,
    guessing: Option<&BTreeSet<&'a str>>,
) {
    for _ in 0..=most.len() {
        let mut lowered = false;
        for limit in bounds {
            for (p, coefficient) in &limit.terms {
                if *coefficient <= 0 || guessing.is_some_and(|g| !g.contains(p)) {
                    continue;
                }
                let Some(least) = least_of_others(limit, p, most, guessing.is_some()) else {
                    continue;
                };
                let cap = limit
                    .bound
                    .saturating_sub(least)
                    .div_euclid(*coefficient)
                    .max(0);
                if most[p].is_none_or(|m| cap < m) {
                    most.insert(*p, Some(cap));
                    lowered = true;
                }
            }
        }
        if !lowered {
            return;
        }
    }
}

/// The least the rest of `limit`, besides `p`, can be: its rest's least,
/// every parameter that adds at 0 and every one that subtracts at its
/// largest. `None` when one that subtracts has no largest yet, unless
/// `unknown_as_zero`.
fn least_of_others(
    limit: &Limit<'_>,
    p: &str,
    most: &BTreeMap<&str, Option<i64>>,
    unknown_as_zero: bool,
) -> Option<i64> {
    let mut least = limit.rest.least;
    for (q, c) in &limit.terms {
        if *c >= 0 || *q == p {
            continue;
        }
        let largest = match most.get(q).copied().flatten() {
            Some(m) => m,
            None if unknown_as_zero => 0,
            None => return None,
        };
        least = least.saturating_add(c.saturating_mul(largest));
    }
    Some(least)
}

/// How far `p` goes to give each list position it reaches the first one
/// past the end, whatever the rest is, and whether that took another
/// parameter nothing limits as 0. `None` when it gives no position.
fn reach(
    limits: &[Limit<'_>],
    most: &BTreeMap<&str, Option<i64>>,
    p: &str,
) -> (Option<usize>, bool) {
    let mut guessed = false;
    let reach = limits
        .iter()
        .filter(|l| l.counter.is_none() && l.coefficient(p) > 0)
        .map(|l| {
            let least = least_of_others(l, p, most, false).unwrap_or_else(|| {
                guessed = true;
                least_of_others(l, p, most, true).unwrap_or(l.rest.least)
            });
            let steps = ceil_div(l.bound.saturating_sub(least), l.coefficient(p));
            usize::try_from(steps).unwrap_or(0)
        })
        .max();
    (reach, guessed)
}

/// How far `p` still changes a form it lowers: until the largest that form
/// can be is at 0. A parameter that adds to it counts at its largest, or at
/// `widest` while nothing limits it. `None` when it lowers no form.
fn lowering_worth(
    limits: &[Limit<'_>],
    most: &BTreeMap<&str, Option<i64>>,
    p: &str,
    widest: usize,
) -> Option<usize> {
    limits
        .iter()
        .filter(|l| l.coefficient(p) < 0)
        .map(|l| {
            let highest =
                l.terms
                    .iter()
                    .filter(|(_, c)| **c > 0)
                    .fold(l.rest.most, |sum, (q, c)| {
                        let largest = most.get(q).copied().flatten().unwrap_or(whole(widest));
                        sum.saturating_add(c.saturating_mul(largest))
                    });
            let steps = ceil_div(highest, l.coefficient(p).saturating_neg());
            usize::try_from(steps).unwrap_or(0)
        })
        .max()
}

/// `n / d` rounded up, for `d > 0`, and 0 for `n <= 0`.
fn ceil_div(n: i64, d: i64) -> i64 {
    if n <= 0 {
        0
    } else {
        n.div_euclid(d) + i64::from(n.rem_euclid(d) != 0)
    }
}

fn whole(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The largest bound of any counter or list: how far a parameter is
/// explored where a counter it reaches could not be worked out.
fn widest(model: &TemperModel) -> usize {
    model
        .counter_bounds
        .values()
        .copied()
        .chain(model.list_bounds.values().copied())
        .fold(model.default_max_counter, usize::max)
}
