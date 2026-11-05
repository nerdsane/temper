//! Counters kept by how far behind the newest they are. A count that a
//! reaction carries from one record to another, which the receiver takes
//! only when it is greater than the one it holds (`param_greater_than_field`),
//! works as a sequence number: what decides it is which copy is newer. The
//! model keeps each such family of counters (the fields, and the parameters
//! in flight that carry them) as whether each is its family's newest or
//! behind it. That decides the constraint for the newest copy and keeps the
//! model small; between two copies that are both behind it is one of taken
//! and refused. A copy in flight that is not greater than a field that never
//! goes down will be refused whenever it lands, so it is kept equal to that
//! field and its refusal stays decided.
//!
//! A family is kept so only when nothing else writes or reads it: no guard
//! or property reads its fields, nothing sets them to a literal or lowers
//! them, and its parameters are sent only by reactions, each carrying one of
//! its fields. Taking a carried copy keeps this exact, and so does adding to
//! the newest. Adding to one behind may or may not make it the newest, so
//! the model does not count on that move.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{ActionConstraint, Automaton};
use temper_spec::predicate::{Arg, Expr, Literal, Name, Truth};

use super::super::JointState;
use super::Reaction;
use crate::model::{Fields, ModelEffect, ParamValue, Sym, TemperModel, TemperModelState};
use rustc_hash::FxHashMap;

mod fan;
mod normal;

pub(super) use normal::values;

/// How many steps behind its family's newest the model tells a count apart;
/// a count further behind is one value.
const BEHIND: usize = 1;

/// A counter field: (entity, counter).
type Field = (String, String);
/// A parameter: (entity, action, parameter).
type Param = (String, String, String);
/// Each (entity, action)'s (parameter, field) pairs, by interned name.
type Refusals = FxHashMap<(Sym, Sym), Box<[(Sym, Sym)]>>;

/// The families of counts kept as sequence numbers.
#[derive(Debug, Default)]
pub(super) struct Order {
    pub(super) families: Vec<Family>,
    /// The families as each state asks of them, by name.
    numbered: Vec<Numbered>,
    /// Each (entity, action)'s `param_greater_than_field` constraints the
    /// model decides, as (parameter, field).
    compared: BTreeMap<(String, String), Vec<(String, String)>>,
    /// What can refuse a dispatch whenever it lands: each (entity, action)'s
    /// (parameter, field) of `compared` whose field is in `rising`.
    refusals: Refusals,
    /// The fields that never go down: each write adds to them, or takes a
    /// count only when it is greater.
    rising: BTreeSet<Field>,
}

#[derive(Debug, Default, Clone)]
pub(super) struct Family {
    fields: BTreeSet<Field>,
    params: BTreeSet<Param>,
    /// The most a move adds to one of its fields.
    step: usize,
}

/// What each state asks of a family: where its values are, by interned name.
#[derive(Debug)]
struct Numbered {
    fields: Box<[(Sym, Sym)]>,
    params: Box<[(Sym, Sym, Sym)]>,
}

impl Numbered {
    fn of(family: &Family) -> Self {
        Self {
            fields: family
                .fields
                .iter()
                .map(|(entity, field)| (Sym::new(entity), Sym::new(field)))
                .collect(),
            params: family
                .params
                .iter()
                .map(|(entity, action, param)| {
                    (Sym::new(entity), Sym::new(action), Sym::new(param))
                })
                .collect(),
        }
    }
}

/// What of `compared` can refuse a dispatch whenever it lands.
fn refusals(
    compared: &BTreeMap<(String, String), Vec<(String, String)>>,
    rising: &BTreeSet<Field>,
) -> Refusals {
    compared
        .iter()
        .filter_map(|((entity, action), pairs)| {
            let pairs: Box<[(Sym, Sym)]> = pairs
                .iter()
                .filter(|(_, field)| rising.contains(&(entity.clone(), field.clone())))
                .map(|(param, field)| (Sym::new(param), Sym::new(field)))
                .collect();
            (!pairs.is_empty()).then(|| ((Sym::new(entity), Sym::new(action)), pairs))
        })
        .collect()
}

/// What the families are read from: the specs, their models before slicing, the
/// reactions between them, what the properties read, and whether the model
/// chooses an action's parameters (someone outside or a module sends it, or
/// the kernel sends it with none).
pub(super) struct Specs<'a> {
    pub(super) by_name: &'a BTreeMap<&'a str, &'a Automaton>,
    pub(super) models: &'a BTreeMap<String, TemperModel>,
    pub(super) reactions: &'a [Reaction],
    pub(super) read: &'a BTreeMap<String, BTreeSet<String>>,
    pub(super) chosen: &'a dyn Fn(&str, &str) -> bool,
}

impl Order {
    /// The families of counts `specs` keep as sequence numbers.
    pub(super) fn read(specs: &Specs<'_>) -> Self {
        let counter = |entity: &str, field: &str| {
            specs.models.get(entity).is_some_and(|m| {
                m.var_kinds.get(field) == Some(&temper_spec::predicate::VarKind::Counter)
            })
        };
        let read = fields_read(specs);
        let mut families: Vec<Family> = Vec::new();
        for entity in specs.models.keys() {
            for action in &specs.by_name[entity.as_str()].actions {
                for constraint in &action.constraints {
                    let ActionConstraint::ParamGreaterThanField { param, field } = constraint
                    else {
                        continue;
                    };
                    let param = (entity.clone(), action.name.clone(), param.clone());
                    if !counter(entity, field) || families.iter().any(|f| f.params.contains(&param))
                    {
                        continue;
                    }
                    let mut family = Family::default();
                    family.fields.insert((entity.clone(), field.clone()));
                    family.params.insert(param);
                    families.push(family);
                }
            }
        }
        let grown: Vec<Option<Family>> = families
            .into_iter()
            .map(|family| grow(family, specs, &counter, &read))
            .collect();
        let families = merge(grown);
        let mut compared: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
        for family in &families {
            for (entity, action, param) in &family.params {
                let constraints = specs.by_name[entity.as_str()]
                    .actions
                    .iter()
                    .filter(|a| &a.name == action)
                    .flat_map(|a| &a.constraints);
                for constraint in constraints {
                    if let ActionConstraint::ParamGreaterThanField { param: p, field } = constraint
                        && p == param
                        && family.fields.contains(&(entity.clone(), field.clone()))
                    {
                        compared
                            .entry((entity.clone(), action.clone()))
                            .or_default()
                            .push((param.clone(), field.clone()));
                    }
                }
            }
        }
        let rising = families
            .iter()
            .flat_map(|family| family.fields.iter())
            .filter(|(entity, field)| {
                specs.models[entity].transitions.iter().all(|t| {
                    t.effects.iter().all(|effect| match effect {
                        ModelEffect::SetCounter {
                            var,
                            value: Arg::Param(param),
                        } if var == field => compared
                            .get(&(entity.clone(), t.name.to_string()))
                            .is_some_and(|pairs| pairs.contains(&(param.clone(), field.clone()))),
                        _ => true,
                    })
                })
            })
            .cloned()
            .collect::<BTreeSet<Field>>();
        Self {
            numbered: families.iter().map(Numbered::of).collect(),
            families,
            refusals: refusals(&compared, &rising),
            compared,
            rising,
        }
    }

    /// The counters of `entity` kept as sequence numbers, each with the largest
    /// value the model gives one.
    pub(super) fn fields<'a>(&'a self, entity: &'a str) -> impl Iterator<Item = (&'a str, usize)> {
        self.families.iter().flat_map(move |family| {
            // The newest, then one more step up.
            let most = BEHIND + family.step;
            family
                .fields
                .iter()
                .filter(move |(e, _)| e == entity)
                .map(move |(_, field)| (field.as_str(), most))
        })
    }

    /// Let `entity`'s counts kept as sequence numbers take every value the
    /// model gives them, so the model does not cut a move that adds to one.
    pub(super) fn bound(&self, entity: &str, model: &mut TemperModel) {
        for (field, most) in self.fields(entity) {
            model.counter_bounds.insert(field.to_string(), most);
        }
    }

    /// Whether `entity`'s `action` parameter `param` carries a counter kept
    /// as a sequence number.
    pub(super) fn carries(&self, entity: &str, action: &str, param: &str) -> bool {
        self.families.iter().any(|f| {
            f.params
                .iter()
                .any(|(e, a, p)| e == entity && a == action && p == param)
        })
    }

    /// Whether the model decides this parameter's comparison with this field.
    pub(super) fn decides(&self, entity: &str, action: &str, param: &str, field: &str) -> bool {
        self.compared
            .get(pair!(entity, action))
            .is_some_and(|pairs| pairs.iter().any(|(p, f)| p == param && f == field))
    }

    /// Whether `params` pass `entity`'s `action` constraints the model
    /// decides as sequence numbers, on `target`, as far as the model knows. A parameter nobody
    /// sent is refused, as the kernel refuses any constrained parameter that
    /// is missing.
    pub(super) fn holds(
        &self,
        entity: &str,
        action: &str,
        params: &Fields<ParamValue>,
        target: &TemperModelState,
    ) -> Truth {
        let Some(pairs) = self.compared.get(pair!(entity, action)) else {
            return Truth::True;
        };
        let mut truth = Truth::True;
        for (param, field) in pairs {
            let held = target.counters.get(field).copied().unwrap_or(0);
            match params.get(param) {
                // Both further behind the newest than the model tells apart.
                Some(ParamValue::Count(0)) if held == 0 => truth = Truth::Unknown,
                Some(ParamValue::Count(sent)) if *sent > held => {}
                _ => return Truth::False,
            }
        }
        truth
    }

    /// Whether the model stays exact when `entity`, in `state`, applies
    /// `effects`: each counter they add to is its family's newest.
    pub(super) fn exact(&self, entity: &str, effects: &[ModelEffect], state: &JointState) -> bool {
        effects.iter().all(|effect| {
            let ModelEffect::AddCounter { var, .. } = effect else {
                return true;
            };
            self.families
                .iter()
                .filter(|family| family.fields.contains(pair!(entity, var)))
                .all(|family| {
                    let value = state.records[entity]
                        .counters
                        .get(var)
                        .copied()
                        .unwrap_or(0);
                    values(family, state).iter().all(|v| *v <= value)
                })
        })
    }
}

/// `family` with every field and parameter that reaches it, or `None` when
/// something else writes or reads it.
fn grow(
    mut family: Family,
    specs: &Specs<'_>,
    counter: &dyn Fn(&str, &str) -> bool,
    read: &BTreeSet<Field>,
) -> Option<Family> {
    loop {
        let before = (family.fields.len(), family.params.len());
        for (entity, action, param) in family.params.clone() {
            if (specs.chosen)(&entity, &action) {
                return None;
            }
            for reaction in specs
                .reactions
                .iter()
                .filter(|r| r.to == entity && r.target_action == action)
            {
                match reaction.args.get(&param) {
                    Some(Arg::Var(field)) if counter(&reaction.from, field) => {
                        family.fields.insert((reaction.from.clone(), field.clone()));
                    }
                    _ => return None,
                }
            }
        }
        for (entity, field) in family.fields.clone() {
            if read.contains(&(entity.clone(), field.clone())) {
                return None;
            }
            for t in &specs.models[&entity].transitions {
                for effect in &t.effects {
                    match effect {
                        ModelEffect::AddCounter {
                            var,
                            value: Arg::Lit(Literal::Int(n)),
                        } if var == &field && *n >= 0 => {
                            family.step = family.step.max(usize::try_from(*n).unwrap_or(0));
                        }
                        ModelEffect::SetCounter {
                            var,
                            value: Arg::Param(param),
                        } if var == &field => {
                            family.params.insert((
                                entity.clone(),
                                t.name.to_string(),
                                param.clone(),
                            ));
                        }
                        ModelEffect::SetCounter { var, .. }
                        | ModelEffect::AddCounter { var, .. }
                        | ModelEffect::SubCounter { var, .. }
                            if var == &field =>
                        {
                            return None;
                        }
                        _ => {}
                    }
                }
            }
            // A copy the field's reactions carry to a parameter that is
            // compared or written joins the family.
            for reaction in specs.reactions.iter().filter(|r| r.from == entity) {
                for (param, arg) in &reaction.args {
                    if matches!(arg, Arg::Var(f) if f == &field)
                        && uses(specs, &reaction.to, &reaction.target_action, param)
                    {
                        family.params.insert((
                            reaction.to.clone(),
                            reaction.target_action.clone(),
                            param.clone(),
                        ));
                    }
                }
            }
        }
        if (family.fields.len(), family.params.len()) == before {
            return Some(family);
        }
    }
}

/// Whether `entity`'s `action` compares its counter parameter `param` with a
/// field or writes a counter from it.
fn uses(specs: &Specs<'_>, entity: &str, action: &str, param: &str) -> bool {
    let compares = specs.by_name.get(entity).is_some_and(|automaton| {
        automaton.actions.iter().any(|a| {
            a.name == action
                && a.constraints.iter().any(|c| {
                    matches!(c, ActionConstraint::ParamGreaterThanField { param: p, .. } if p == param)
                })
        })
    });
    let writes = specs.models.get(entity).is_some_and(|model| {
        model.transitions.iter().any(|t| {
            t.name == action
                && t.effects.iter().any(|e| {
                    matches!(e, ModelEffect::SetCounter { value: Arg::Param(p), .. } if p == param)
                })
        })
    });
    compares || writes
}

/// Families that share a field or parameter become one; one that could not
/// be kept spoils those it meets.
fn merge(grown: Vec<Option<Family>>) -> Vec<Family> {
    let mut out: Vec<(Family, bool)> = Vec::new();
    for family in grown {
        let (family, kept) = match family {
            Some(family) => (family, true),
            None => (Family::default(), false),
        };
        let (mut joined, mut kept) = (family, kept);
        let mut rest = Vec::new();
        for (other, other_kept) in out {
            let meets = !joined.fields.is_disjoint(&other.fields)
                || !joined.params.is_disjoint(&other.params);
            if meets {
                joined.fields.extend(other.fields);
                joined.params.extend(other.params);
                joined.step = joined.step.max(other.step);
                kept &= other_kept;
            } else {
                rest.push((other, other_kept));
            }
        }
        rest.push((joined, kept));
        out = rest;
    }
    out.into_iter()
        .filter(|(family, kept)| *kept && !family.fields.is_empty())
        .map(|(family, _)| family)
        .collect()
}

/// The fields read by guards, properties, or effect arguments.
fn fields_read(specs: &Specs<'_>) -> BTreeSet<Field> {
    let mut read: BTreeSet<Field> = specs
        .read
        .iter()
        .flat_map(|(entity, fields)| fields.iter().map(move |f| (entity.clone(), f.clone())))
        .collect();
    let mut note = |owner: &str, expr: &Expr| {
        expr.for_each_name(&mut |name| match name {
            Name::Var(var) => {
                read.insert((owner.to_string(), var.to_string()));
            }
            Name::CrossField {
                entity_type, field, ..
            } => {
                read.insert((entity_type.to_string(), field.to_string()));
            }
            _ => {}
        });
    };
    for (entity, model) in specs.models {
        for t in &model.transitions {
            note(entity, &t.guard);
            for effect in &t.effects {
                if let Some(field) = super::super::slice::effect_reads(effect) {
                    note(entity, &Expr::Var(field.to_string()));
                }
            }
        }
    }
    for reaction in specs.reactions {
        if let Some(guard) = &reaction.guard {
            note(&reaction.from, guard);
        }
    }
    read
}
