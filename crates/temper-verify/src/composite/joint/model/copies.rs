//! Holding more than one record of a type, so two writers really race.
//!
//! The model is read once, per type, and then each table it read is copied for
//! each further record of the type: `Alert`, then `Alert#2`. A move, a timer or
//! a reaction belongs to one named record already, so the walk needs nothing
//! new. A reaction goes from each record to each record of its target's type
//! and lands where the ids match, and a reference finds its record by id
//! among the copies.
//!
//! Which copy holds which record means nothing: only ids tell records apart,
//! and they are only compared. So states that differ by which copy holds what
//! are one state, and [`JointModel::in_one_order`] walks them once.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use temper_spec::automaton::TargetResolver;
use temper_spec::predicate::{Expr, Truth, eval};

use super::super::JointState;
use super::super::env::ID;
use super::JointModel;
use crate::model::{ModelString, TemperModelState};

/// The type a record's name is of: copies are named `Type#n`, and a type's
/// name holds no `#`.
pub(in crate::composite::joint) fn base(entity: &str) -> &str {
    entity.split_once('#').map_or(entity, |(base, _)| base)
}

/// The tables below hold every record's name.
fn fan<V: Clone>(table: &mut BTreeMap<String, V>, copies: &BTreeMap<String, Vec<String>>) {
    for (entity, names) in copies {
        if let Some(value) = table.get(entity).cloned() {
            for name in &names[1..] {
                table.insert(name.clone(), value.clone());
            }
        }
    }
}

pub(in crate::composite::joint) fn fan_moves<V: Clone>(
    table: &mut BTreeMap<(String, String), V>,
    copies: &BTreeMap<String, Vec<String>>,
) {
    let found: Vec<_> = table
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for ((entity, action), value) in found {
        for name in copies.get(&entity).into_iter().flatten().skip(1) {
            table.insert((name.clone(), action.clone()), value.clone());
        }
    }
}

pub(in crate::composite::joint) fn fan_set(
    set: &mut BTreeSet<(String, String)>,
    copies: &BTreeMap<String, Vec<String>>,
) {
    let found: Vec<_> = set.iter().cloned().collect();
    for (entity, second) in found {
        for name in copies.get(&entity).into_iter().flatten().skip(1) {
            set.insert((name.clone(), second.clone()));
        }
    }
}

impl JointModel {
    /// This model holding `records` records of each type in scope.
    pub(in crate::composite::joint) fn with_copies(mut self, records: usize) -> Self {
        if records <= 1 {
            return self;
        }
        self.copies = self
            .models
            .keys()
            .map(|entity| {
                let names = std::iter::once(entity.clone())
                    .chain((2..=records).map(|n| format!("{entity}#{n}")))
                    .collect();
                (entity.clone(), names)
            })
            .collect();
        let copies = std::mem::take(&mut self.copies);
        fan(&mut self.models, &copies);
        fan(&mut self.refs, &copies);
        fan(&mut self.id_literals, &copies);
        fan(&mut self.unsent, &copies);
        fan(&mut self.kernel.timers, &copies);
        fan(&mut self.kernel.internal, &copies);
        fan_moves(&mut self.kernel.calls_on, &copies);
        fan_moves(&mut self.outside_spawns, &copies);
        fan_moves(&mut self.outside, &copies);
        fan_set(&mut self.relies_on, &copies);
        for (entity, names) in &copies {
            if self.sender_matters.contains(entity) {
                self.sender_matters.extend(names.iter().cloned());
            }
        }
        self.unchecked.fan(&copies);
        self.order.fan(&copies);
        self.fan_reactions(&copies);
        self.several_copies = copies.values().any(|names| names.len() > 1);
        self.copies = copies;
        self
    }

    /// One reaction from each record of its source's type to each record of
    /// its target's.
    fn fan_reactions(&mut self, copies: &BTreeMap<String, Vec<String>>) {
        let reactions = std::mem::take(&mut self.reactions);
        let chosen = std::mem::take(&mut self.chosen_guards);
        for (first, reaction) in reactions.into_iter().enumerate() {
            for from in copies[&reaction.from].iter() {
                for to in copies[&reaction.to].iter() {
                    // A record's trigger on its own id reaches that record,
                    // never another of its type: their ids differ.
                    if reaction.from == reaction.to
                        && from != to
                        && matches!(reaction.resolve, Some(TargetResolver::SameId))
                    {
                        continue;
                    }
                    let at = self.reactions.len();
                    self.reaction_at
                        .insert((first, from.clone(), to.clone()), at);
                    self.first_reaction.push(first);
                    if chosen.contains(&first) {
                        self.chosen_guards.insert(at);
                    }
                    let mut copy = reaction.clone();
                    copy.from.clone_from(from);
                    copy.to.clone_from(to);
                    self.reactions.push(copy);
                }
            }
        }
    }

    /// What `rule` of `owner`'s type says of every record of it in `state`.
    pub(super) fn rule_truth(&self, owner: &str, rule: &Expr, state: &JointState) -> Truth {
        let mut truth = Truth::True;
        for name in self.records_of(owner) {
            match eval(rule, &self.env(name, state)) {
                Truth::False => return Truth::False,
                Truth::Unknown => truth = Truth::Unknown,
                Truth::True => {}
            }
        }
        truth
    }

    /// How many records of each type the model holds.
    pub(in crate::composite::joint) fn records_held(&self) -> usize {
        self.copies.values().map(Vec::len).max().unwrap_or(1)
    }

    /// The names of `entity`'s records.
    pub(in crate::composite::joint) fn records_of(&self, entity: &str) -> &[String] {
        &self.copies[entity]
    }

    /// The copy of reaction `index` that makes the record, decided once as
    /// `state` holds it when the first copy comes up; `None` for the others.
    pub(super) fn making_takes(
        &self,
        state: &JointState,
        source: &str,
        index: usize,
        named: Option<&ModelString>,
    ) -> Option<usize> {
        let child = self.reactions[index].to.as_str();
        let names = &self.copies[base(child)];
        if names.len() == 1 {
            return Some(index);
        }
        (names[0] == child).then(|| {
            let takes = self.spawn_takes(state, source, child, named);
            self.reaction_at[&(
                self.first_reaction[index],
                source.to_string(),
                takes.to_string(),
            )]
        })
    }

    /// The record of `child`'s type a making takes: the one with the id it
    /// names, or else the first that hasn't started and isn't `source`, or
    /// else the first with nothing left to lose, or else the first but
    /// `source`, which is past the bound.
    fn spawn_takes<'a>(
        &'a self,
        state: &JointState,
        source: &str,
        child: &'a str,
        named: Option<&ModelString>,
    ) -> &'a str {
        let names = &self.copies[base(child)];
        if let Some(named) = named
            && let Some(name) = names
                .iter()
                .find(|name| state.records[name.as_str()].strings.get(ID) == Some(named))
        {
            return name;
        }
        names
            .iter()
            .find(|name| *name != source && self.unstarted(name, &state.records[name.as_str()]))
            .or_else(|| {
                names
                    .iter()
                    .find(|name| *name != source && self.replaces_cleanly(state, name))
            })
            .or_else(|| names.iter().find(|name| *name != source))
            .unwrap_or(&names[0])
    }

    /// `state` with the copies of each type in one order. Any order of them
    /// is the same state, so this loses nothing, and states that differ only
    /// by it become one. Records that compare equal here keep their order, so
    /// not every such state is caught, which only costs walking a state twice.
    #[inline]
    pub(super) fn in_one_order(&self, state: &mut JointState) {
        if self.several_copies {
            self.order_copies(state);
        }
    }

    fn order_copies(&self, state: &mut JointState) {
        let mut moved: BTreeMap<&str, &str> = BTreeMap::new();
        let mut holds: Vec<(&str, std::sync::Arc<TemperModelState>, Option<u32>)> = Vec::new();
        for names in self.copies.values().filter(|names| names.len() > 1) {
            let mut order: Vec<&String> = names.iter().collect();
            order.sort_by(|a, b| {
                compare(
                    &state.records[a.as_str()],
                    &state.records[b.as_str()],
                    state.inflight.iter().filter(|d| &d.to == *a).count(),
                    state.inflight.iter().filter(|d| &d.to == *b).count(),
                )
            });
            for (name, was) in names.iter().zip(order) {
                if name != was {
                    moved.insert(was, name);
                    holds.push((
                        name,
                        state.records[was.as_str()].clone(),
                        state.armed.get(was.as_str()).copied(),
                    ));
                }
            }
        }
        if moved.is_empty() {
            return;
        }
        for (name, record, armed) in holds {
            state.records.insert(name, record);
            match armed {
                Some(by) => state.armed.insert(name, by),
                None => state.armed.remove(name),
            };
        }
        let renamed = |name: &str| moved.get(name).copied();
        for dispatch in &mut state.inflight {
            let via = dispatch.via.and_then(|at| {
                let reaction = &self.reactions[at];
                let (from, to) = (reaction.from.as_str(), reaction.to.as_str());
                let (new_from, new_to) = (renamed(from), renamed(to));
                (new_from.is_some() || new_to.is_some()).then(|| {
                    self.reaction_at[&(
                        self.first_reaction[at],
                        new_from.unwrap_or(from).to_string(),
                        new_to.unwrap_or(to).to_string(),
                    )]
                })
            });
            let to = renamed(&dispatch.to);
            if via.is_some() || to.is_some() {
                let dispatch = std::sync::Arc::make_mut(dispatch);
                if let Some(via) = via {
                    dispatch.via = Some(via);
                }
                if let Some(to) = to {
                    dispatch.to = to.into();
                }
            }
        }
        state.inflight.sort();
        for call in &mut state.calls {
            if let Some(to) = renamed(&call.to) {
                std::sync::Arc::make_mut(call).to = to.into();
            }
        }
        state.calls.sort();
    }
}

/// An order of records that doesn't read which values a new string holds,
/// which have no order that survives renaming.
fn compare(
    a: &TemperModelState,
    b: &TemperModelState,
    a_waiting: usize,
    b_waiting: usize,
) -> Ordering {
    let plain = |value: &ModelString| {
        !matches!(value, ModelString::Symbol(_) | ModelString::BlankSymbol(_))
    };
    let strings = |record: &TemperModelState| {
        record
            .strings
            .iter()
            .filter(|(field, _)| field.as_str() != ID)
            .map(|(field, value)| (*field, plain(value).then_some(*value)))
            .collect::<Vec<_>>()
    };
    a.status
        .cmp(&b.status)
        .then_with(|| a.counters.cmp(&b.counters))
        .then_with(|| a.booleans.cmp(&b.booleans))
        .then_with(|| a_waiting.cmp(&b_waiting))
        .then_with(|| strings(a).cmp(&strings(b)))
}
