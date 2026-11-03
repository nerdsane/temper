//! A counter's value partway through a move: the largest of a few linear
//! forms over the move's count parameters and what the counters held before
//! it, each plus a constant part.

use std::collections::{BTreeMap, BTreeSet};

use super::whole;
use crate::model::semantics::{MAX_PARAM_ASSIGNMENTS, counter_bound};
use crate::model::types::TemperModel;

/// Names in a linear form, each with its whole-number coefficient.
pub(super) type Coefficients<'a> = BTreeMap<&'a str, i64>;

/// The variable part of a linear form. Reading the same counter twice gives
/// the same term, so `x += z; x -= z` cancels as `x = a; x -= a` does.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Terms<'a> {
    /// The move's count parameters.
    pub(super) params: Coefficients<'a>,
    /// Counters as they were before the move.
    pub(super) before: Coefficients<'a>,
}

/// The constant part of a form: the least and the most it can be. Exact for
/// literals; a range once terms for counters before the move are folded
/// into it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Rest {
    pub(super) least: i64,
    pub(super) most: i64,
}

/// How many sums one addition or subtraction may work out. Each counter that
/// stops at 0 doubles the forms of what it is added to, so a contrived chain
/// of them would fill memory before the assignment budget refuses the move,
/// and no spec writes as many as a move explores assignments. Past it the
/// counters before the move are folded into ranges, which leaves one form
/// per sum of parameters; past it still, the value is marked as not worked
/// out, so the move is still explored, widened and reported as bounded.
const MAX_FORMS: usize = MAX_PARAM_ASSIGNMENTS;

/// A counter's value partway through the move.
#[derive(Clone, Debug)]
pub(super) enum Value<'a> {
    /// The largest of these forms, each its terms plus its rest.
    Largest(BTreeMap<Terms<'a>, Rest>),
    /// A value that could not be worked out, with the count parameters it
    /// depends on.
    Unknown(BTreeSet<&'a str>),
}

impl<'a> Value<'a> {
    pub(super) fn rest(least: i64, most: i64) -> Self {
        Value::Largest(BTreeMap::from([(Terms::default(), Rest { least, most })]))
    }

    pub(super) fn param(p: &'a str) -> Self {
        Self::term(Terms {
            params: Coefficients::from([(p, 1)]),
            before: Coefficients::new(),
        })
    }

    pub(super) fn before(counter: &'a str) -> Self {
        Self::term(Terms {
            params: Coefficients::new(),
            before: Coefficients::from([(counter, 1)]),
        })
    }

    fn term(terms: Terms<'a>) -> Self {
        Value::Largest(BTreeMap::from([(terms, Rest { least: 0, most: 0 })]))
    }

    fn params(&self) -> BTreeSet<&'a str> {
        match self {
            Value::Largest(forms) => forms
                .keys()
                .flat_map(|terms| terms.params.keys().copied())
                .collect(),
            Value::Unknown(params) => params.clone(),
        }
    }

    /// `self + sign * other`: each form of one plus `sign` times each form
    /// of the other, since the larger of sums is the sum of the larger.
    pub(super) fn combine(&self, other: &Value<'a>, sign: i64, model: &TemperModel) -> Value<'a> {
        let (Value::Largest(ours), Value::Largest(theirs)) = (self, other) else {
            return Value::Unknown(&self.params() | &other.params());
        };
        if ours.len().saturating_mul(theirs.len()) <= MAX_FORMS {
            return Value::Largest(sums(ours, theirs, sign));
        }
        let (ours, theirs) = (fold_before(model, ours), fold_before(model, theirs));
        if ours.len().saturating_mul(theirs.len()) <= MAX_FORMS {
            return Value::Largest(sums(&ours, &theirs, sign));
        }
        Value::Unknown(&self.params() | &other.params())
    }

    /// `self - other`, stopping at 0 as L1 and the runtime do.
    pub(super) fn take(&self, other: &Value<'a>, model: &TemperModel) -> Value<'a> {
        let one = match other {
            Value::Largest(forms) if forms.len() == 1 => Some(other.clone()),
            Value::Largest(forms) => {
                let folded = fold_before(model, forms);
                (folded.len() == 1).then_some(Value::Largest(folded))
            }
            Value::Unknown(_) => None,
        };
        // Taking the larger of several values leaves the smaller of
        // several, which is not the larger of any forms.
        let Some(one) = one else {
            return Value::Unknown(&self.params() | &other.params());
        };
        match self.combine(&one, -1, model) {
            Value::Largest(mut left) => {
                keep_largest(&mut left, Terms::default(), Rest { least: 0, most: 0 });
                Value::Largest(left)
            }
            unknown @ Value::Unknown(_) => unknown,
        }
    }
}

/// Each form of `ours` plus `sign` times each form of `theirs`.
fn sums<'a>(
    ours: &BTreeMap<Terms<'a>, Rest>,
    theirs: &BTreeMap<Terms<'a>, Rest>,
    sign: i64,
) -> BTreeMap<Terms<'a>, Rest> {
    let mut sums = BTreeMap::new();
    for (our_terms, ours) in ours {
        for (their_terms, theirs) in theirs {
            let terms = Terms {
                params: add(&our_terms.params, &their_terms.params, sign),
                before: add(&our_terms.before, &their_terms.before, sign),
            };
            let rest = if sign > 0 {
                Rest {
                    least: ours.least.saturating_add(theirs.least),
                    most: ours.most.saturating_add(theirs.most),
                }
            } else {
                Rest {
                    least: ours.least.saturating_sub(theirs.most),
                    most: ours.most.saturating_sub(theirs.least),
                }
            };
            keep_largest(&mut sums, terms, rest);
        }
    }
    sums
}

/// `ours + sign * theirs`, without the names whose coefficients cancel.
fn add<'a>(ours: &Coefficients<'a>, theirs: &Coefficients<'a>, sign: i64) -> Coefficients<'a> {
    let mut sum = ours.clone();
    for (name, coefficient) in theirs {
        let entry = sum.entry(*name).or_insert(0);
        *entry = entry.saturating_add(coefficient.saturating_mul(sign));
    }
    sum.retain(|_, coefficient| *coefficient != 0);
    sum
}

/// `forms` with what each counter held before the move folded into the
/// constant part, as the least and the most it can make it, given that the
/// counter lies between 0 and its bound. The range only loses which counter
/// a value came from, which widens limits and explores more, never less.
pub(super) fn fold_before<'a>(
    model: &TemperModel,
    forms: &BTreeMap<Terms<'a>, Rest>,
) -> BTreeMap<Terms<'a>, Rest> {
    let mut folded = BTreeMap::new();
    for (terms, rest) in forms {
        let mut rest = *rest;
        for (counter, coefficient) in &terms.before {
            let reach = coefficient.saturating_mul(whole(counter_bound(model, counter)));
            if reach > 0 {
                rest.most = rest.most.saturating_add(reach);
            } else {
                rest.least = rest.least.saturating_add(reach);
            }
        }
        let params = Terms {
            params: terms.params.clone(),
            before: Coefficients::new(),
        };
        keep_largest(&mut folded, params, rest);
    }
    folded
}

/// Add a form to `forms`. Two forms with the same terms make one, their
/// larger, whose rest is at least the larger least and at most the larger
/// most.
fn keep_largest<'a>(forms: &mut BTreeMap<Terms<'a>, Rest>, terms: Terms<'a>, rest: Rest) {
    forms
        .entry(terms)
        .and_modify(|kept| {
            kept.least = kept.least.max(rest.least);
            kept.most = kept.most.max(rest.most);
        })
        .or_insert(rest);
}
