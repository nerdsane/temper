//! A spawn: a move makes a record of another type, stores its id on the
//! record that moved, and sends the new record its first action.
//!
//! The model holds one record of each type, so a spawn makes the record the
//! model holds of the child's type. When that record has not started, the new
//! record simply takes its place. When one is already there, the runtime
//! would hold both. The model is sure of the replacement only if the old
//! record can do nothing more and nothing else names it; otherwise it walks
//! both ways as unsure: the new record replaces it, or the old one carries on.
//!
//! The runtime makes the child after its parent commits, so for a moment the
//! parent names a record that does not exist yet. The model makes it at the
//! commit and does not walk that moment.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Action, Automaton, Effect};
use temper_spec::predicate::{Arg, Literal, Truth};

use super::super::env::{Blind, ID, ONE_ID, Records};
use super::super::state::ReactionStep;
use super::super::{Dispatch, JointState};
use super::JointModel;
use super::caller::Sent;
use super::commit::add;
use super::read::Reaction;
use super::step::Certainty;
use crate::model::{Fields, ModelString, ParamValue, TemperModelState, fresh_string};

/// What a [`Reaction`] that stands for a spawn does beyond sending an action.
#[derive(Debug, Clone)]
pub(super) struct Spawn {
    /// The field of the parent that holds the child's id.
    store_id_in: Option<String>,
    /// The child's id, when the spawn names one and it is present.
    id: Option<Arg>,
    /// What the kernel gives every child about its parent. These win over
    /// the action's parameters and the copied fields.
    parent: Vec<(String, Arg)>,
    /// Whether the child takes only what its first action declares, so it is
    /// made with nothing.
    strict: bool,
}

/// Each spawn effect of `action` as (child type, first action, copied fields,
/// store field, id).
pub(super) fn spawns_of(
    action: &Action,
) -> impl Iterator<Item = (&String, &String, &[String], &Option<String>, &Option<Arg>)> {
    action.effect.iter().filter_map(|effect| match effect {
        Effect::Spawn {
            entity_type,
            initial_action,
            store_id_in,
            id,
            copy,
        } => Some((
            entity_type,
            initial_action,
            copy.as_slice(),
            store_id_in,
            id,
        )),
        _ => None,
    })
}

/// The reactions that stand for the spawns of `from`'s `action` into `scope`.
/// No Cedar rule decides a child's first action: the parent's move was the
/// decision, and the child's action goes straight to dispatch.
pub(super) fn spawn_reactions(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    from: &str,
    action: &Action,
) -> Vec<Reaction> {
    spawns_of(action)
        .filter(|(to, ..)| scope.contains(*to))
        .map(|(to, target_action, copy, store_id_in, id)| {
            let parent = vec![
                (
                    "parent_type".to_string(),
                    Arg::Lit(Literal::Str(from.to_string())),
                ),
                ("parent_id".to_string(), Arg::Var(ID.to_string())),
                (format!("{}_id", snake(from)), Arg::Var(ID.to_string())),
            ];
            let mut args: BTreeMap<String, Arg> = copy
                .iter()
                .map(|field| (field.clone(), Arg::Var(field.clone())))
                .collect();
            args.extend(parent.iter().cloned());
            Reaction {
                from: from.to_string(),
                source_action: action.name.clone(),
                to: to.clone(),
                target_action: target_action.clone(),
                guard: None,
                args,
                resolve: None,
                principal: None,
                trigger: "spawn".to_string(),
                may_drop: true,
                spawn: Some(Spawn {
                    store_id_in: store_id_in.clone(),
                    id: id.clone(),
                    parent,
                    strict: by_name[to.as_str()].automaton.strict_action_params,
                }),
            }
        })
        .collect()
}

/// `<parent>_id`'s prefix, as the kernel names it.
fn snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (at, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if at > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// A value no record, dispatch or call in `state` holds: a new record's id.
fn minted(model: &JointModel, state: &JointState, entity: &str, field: &str) -> ModelString {
    let mut values = model.values(state);
    values.extend(state.calls.iter().flat_map(|c| c.asked.values().cloned()));
    fresh_string(&values, model.class_of(entity, field), false)
}

impl JointModel {
    /// Each type's record as it first stands. Spawns read it only after the
    /// model is cut down, since nothing walks before.
    pub(super) fn first_records(&self) -> &BTreeMap<String, TemperModelState> {
        self.first.get_or_init(|| {
            self.blank()
                .iter()
                .map(|(entity, record)| (entity.to_string(), (**record).clone()))
                .collect()
        })
    }

    /// Each type's statuses from which its record can send nothing more.
    pub(super) fn settled_statuses(&self) -> &BTreeMap<String, BTreeSet<String>> {
        self.settled.get_or_init(|| settled_statuses(self))
    }

    /// Store new ids for what `source`'s `action` spawns outside the scope.
    /// Nothing the model holds has such an id.
    pub(super) fn store_outside_spawns(&self, state: &mut JointState, source: &str, action: &str) {
        for field in self
            .outside_spawns
            .get(pair!(source, action))
            .into_iter()
            .flatten()
        {
            let id = minted(self, state, source, field);
            self.write(state, source, field, id);
        }
    }

    /// The ways `source`'s spawn `reaction` (`index` among the reactions)
    /// goes after its action, with `params`, committed.
    pub(super) fn spawn(
        &self,
        state: JointState,
        source: &str,
        (index, reaction, spawn): (usize, &Reaction, &Spawn),
        sent: Sent<'_>,
    ) -> Vec<(JointState, Certainty)> {
        let (chain, by, params, caller) = (sent.chain, sent.by, sent.params, sent.caller);
        self.expansion.note_flight_read();
        let child = reaction.to.as_str();
        // A record that makes another of its own type: the model holds one
        // of a type, so the new one is not held and its moves are not walked.
        if child == source {
            let mut next = state;
            let id = minted(self, &next, child, ID);
            if let Some(field) = &spawn.store_id_in {
                self.write(&mut next, source, field, id);
            }
            return vec![(next, Certainty::Unsure)];
        }
        let child_type = super::copies::base(child);
        let step = ReactionStep {
            entity_type: child_type.into(),
            action: reaction.target_action.as_str().into(),
            generated_for: Some(child_type.into()),
        };
        // A first action its chain already holds is a loop: no child, and
        // the field goes back.
        let repeats = self.repeats(chain, &step);
        let mut ways = Vec::new();
        if repeats != Truth::False {
            let mut refused = state.clone();
            self.take_back_id(&mut refused, source, spawn);
            ways.push((
                refused,
                Certainty::unless(repeats == Truth::True, Certainty::OneOf),
            ));
        }
        if repeats == Truth::True {
            return ways;
        }
        let decided = Certainty::unless(repeats == Truth::False, Certainty::OneOf);
        let mut next_chain = chain.to_vec();
        next_chain.push(step);
        let send = |mut next: JointState| {
            let mut sent = self.args(&next, source, reaction);
            let target = &self.models[child];
            for (param, value) in params {
                if target.models_string(&reaction.target_action, param)
                    && !spawn.parent.iter().any(|(name, _)| name == param)
                {
                    sent.insert(*param, *value);
                }
            }
            add(
                &mut next,
                Dispatch {
                    to: child.into(),
                    action: reaction.target_action.as_str().into(),
                    target_state: None,
                    params: sent,
                    read: !self.action_reads_related(child, &reaction.target_action),
                    chain: next_chain.as_slice().into(),
                    via: Some(index),
                    caller: caller.cloned(),
                    unread_guard: None,
                    by: self.senders.reaction(None, by, false),
                },
            );
            next
        };
        let held = state.records[child].strings.get(ID).cloned();
        let named = self.named_id(spawn, params);
        // A spawn naming the record that exists sends it the action.
        if named.is_some() && named == held {
            let mut next = state;
            if self.unstarted(child, &next.records[child]) {
                self.give_parent_fields(&mut next, source, child, spawn);
            }
            self.store_child_id(&mut next, source, child, spawn);
            ways.push((send(next), decided));
            return ways;
        }
        let replaced = self.replaces_cleanly(&state, child);
        let mut next = state.clone();
        let id = named.unwrap_or_else(|| minted(self, &state, child, ID));
        self.start_child(&mut next, child, id);
        self.give_parent_fields(&mut next, source, child, spawn);
        self.store_child_id(&mut next, source, child, spawn);
        let nothing_names_old = held.as_ref().is_none_or(|old| {
            self.unstarted(child, &state.records[child]) || !self.values(&next).contains(old)
        });
        if replaced && nothing_names_old {
            ways.push((send(next), decided));
            return ways;
        }
        // With several records of a type, one more than the bound is past
        // what the walk answers for, as a verdict for up to that many says.
        if self.copies[child_type].len() > 1 {
            return ways;
        }
        ways.push((send(next), Certainty::Unsure));
        let mut carries_on = state;
        self.store_child_id(&mut carries_on, source, child, spawn);
        ways.push((carries_on, Certainty::Unsure));
        ways
    }

    /// What follows when the spawn that sent `dispatch` has its first action
    /// refused, in `state`, where it is no longer in flight. A child that
    /// already existed just refuses. A new child's inputs are checked before
    /// it is made: if they fail, there is no child and the parent's field
    /// goes back. Any other refusal leaves it in its first state, with that
    /// state's timer armed.
    pub(super) fn spawn_refused(&self, state: &mut JointState, dispatch: &Dispatch) {
        let Some(reaction) = dispatch.via.map(|index| &self.reactions[index]) else {
            return;
        };
        let Some(spawn) = &reaction.spawn else {
            return;
        };
        self.expansion.note_flight_read();
        let child = dispatch.to.as_str();
        let record = &state.records[child];
        if !self.unstarted(child, record) {
            return;
        }
        let inputs = self
            .transitions_for(dispatch)
            .any(|t| self.enabled(child, t, state, Blind::Related));
        if inputs {
            let names_it = spawn.store_id_in.as_ref().is_some_and(|field| {
                state.records[reaction.from.as_str()].strings.get(field) == record.strings.get(ID)
            });
            if names_it {
                self.take_back_id(state, &reaction.from, spawn);
            }
        } else if self.timeout(child, &record.status).is_some() {
            let stake = self.senders.stake(dispatch.by, child, record);
            state
                .armed
                .insert(child, stake.arm(self.armer(child, dispatch.by)));
        }
    }

    /// Whether `record` of `entity` is as the type's first state holds it:
    /// no record the runtime has made yet, whatever its id.
    pub(super) fn unstarted(&self, entity: &str, record: &TemperModelState) -> bool {
        let mut plain = record.clone();
        plain.strings.remove(ID);
        plain == self.first_records()[entity]
    }

    /// Whether a new record can take the place of the one `state` holds of
    /// `child` with nothing lost: it has not started, or it can send nothing
    /// more, with nothing on its way to it.
    pub(super) fn replaces_cleanly(&self, state: &JointState, child: &str) -> bool {
        self.expansion.note_flight_read();
        let working = state.inflight.iter().any(|d| d.to == child)
            || state.calls.iter().any(|c| c.to == child);
        if working {
            return false;
        }
        let record = &state.records[child];
        self.unstarted(child, record)
            || (!state.armed.contains_key(child)
                && self.settled_statuses()[child].contains(record.status.as_str()))
    }

    /// Put a new record with `id` in place of what `state` holds of `child`.
    /// Whatever was on its way to the old one stays with the old one.
    fn start_child(&self, state: &mut JointState, child: &str, id: ModelString) {
        let mut record = self.first_records()[child].clone();
        if self.records == Records::ById {
            record.strings.insert(ID.to_string(), id);
        }
        state.records.insert(child, record.into());
        state.armed.remove(child);
        state.inflight.retain(|d| d.to != child);
        state.calls.retain(|c| c.to != child);
    }

    /// Give a child made with `spawn` the fields the kernel makes it with.
    fn give_parent_fields(&self, state: &mut JointState, source: &str, child: &str, spawn: &Spawn) {
        if spawn.strict {
            return;
        }
        for (field, arg) in &spawn.parent {
            let value = match arg {
                Arg::Lit(Literal::Str(text)) => Some(ModelString::from(text.as_str())),
                Arg::Var(var) => state.records[source].strings.get(var).cloned(),
                _ => None,
            };
            if let Some(value) = value {
                self.write(state, child, field, value);
            }
        }
    }

    /// The id `spawn` names, when it names one the model holds.
    pub(super) fn named_id(
        &self,
        spawn: &Spawn,
        params: &Fields<ParamValue>,
    ) -> Option<ModelString> {
        match spawn.id.as_ref()? {
            Arg::Param(param) => match params.get(param) {
                Some(ParamValue::Str(value)) => Some(*value),
                _ => None,
            },
            Arg::Lit(Literal::Str(text)) => Some(ModelString::from(text.as_str())),
            _ => None,
        }
    }

    /// Store the id of the record `state` holds of `child` on `source`.
    fn store_child_id(&self, state: &mut JointState, source: &str, child: &str, spawn: &Spawn) {
        let Some(field) = &spawn.store_id_in else {
            return;
        };
        let id = if self.records == Records::One {
            Some(ModelString::from(ONE_ID))
        } else {
            state.records[child].strings.get(ID).cloned()
        };
        if let Some(id) = id {
            self.write(state, source, field, id);
        }
    }

    /// Put `source`'s store field back to what its first state holds.
    fn take_back_id(&self, state: &mut JointState, source: &str, spawn: &Spawn) {
        let Some(field) = &spawn.store_id_in else {
            return;
        };
        if let Some(value) = self.first_records()[source].strings.get(field).cloned() {
            self.write(state, source, field, value);
        }
    }

    /// Write `field` of `entity`'s record, when the model tracks it.
    fn write(&self, state: &mut JointState, entity: &str, field: &str, value: ModelString) {
        if self.models[entity].tracked_strings.contains(field)
            && let Some(record) = state.records.get_mut(entity)
        {
            std::sync::Arc::make_mut(record)
                .strings
                .insert(field.to_string(), value);
        }
    }
}

/// Each type's statuses from which its record can send nothing more: no
/// reaction, spawn, module call or timer from there on.
pub(super) fn settled_statuses(model: &JointModel) -> BTreeMap<String, BTreeSet<String>> {
    let mut settled = BTreeMap::new();
    for (entity, automaton) in &model.models {
        let sends = |action: &str| {
            model
                .reactions
                .iter()
                .any(|r| r.from == *entity && r.source_action == action)
                || model
                    .kernel
                    .calls_on
                    .contains_key(&(entity.clone(), action.to_string()))
        };
        let mut live: BTreeSet<&str> = model
            .kernel
            .timers
            .get(entity)
            .into_iter()
            .flatten()
            .map(|timer| timer.state.as_str())
            .collect();
        for t in &automaton.transitions {
            if sends(&t.name) {
                live.extend(t.from_states.iter().map(|s| s.as_str()));
            }
        }
        loop {
            let before = live.len();
            for t in &automaton.transitions {
                if t.to_state.as_deref().is_some_and(|to| live.contains(to)) {
                    live.extend(t.from_states.iter().map(|s| s.as_str()));
                }
            }
            if live.len() == before {
                break;
            }
        }
        settled.insert(
            entity.clone(),
            automaton
                .states
                .iter()
                .filter(|status| !live.contains(status.as_str()))
                .cloned()
                .collect(),
        );
    }
    settled
}

/// The fields moves of types in `scope` store ids in for children outside it.
pub(super) fn outside_spawns(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
) -> BTreeMap<(String, String), Vec<String>> {
    let mut stored: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for entity in scope {
        for action in &by_name[entity.as_str()].actions {
            for (to, _, _, field, _) in spawns_of(action) {
                if let (false, Some(field)) = (scope.contains(to), field) {
                    stored
                        .entry((entity.clone(), action.name.clone()))
                        .or_default()
                        .push(field.clone());
                }
            }
        }
    }
    stored
}

#[cfg(test)]
#[path = "spawn_test.rs"]
mod tests;
