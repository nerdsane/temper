//! The drop check (ADR-0150) on the joint model: a reaction whose target
//! refuses it when it lands is dropped, and the runtime says nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use temper_spec::automaton::{Automaton, TargetResolver};
use temper_spec::predicate::Truth;

use super::super::JointState;
use super::super::env::Records;
use super::super::room::Bound;
use super::commit::Arrival;
use super::step::{Move, remove};
use super::{Delivery, JointModel, Keep, Sending, tracked};

/// A reaction its target refused when it landed: the target was not in a
/// state its action is allowed from, or a guard or constraint said no.
/// Reactions that create their target, and `drop_ok` triggers, are not
/// named.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DroppedReaction {
    /// The entity whose action fired the reaction.
    pub source_entity: String,
    /// That action.
    pub source_action: String,
    /// The trigger's name.
    pub trigger_name: String,
    /// The entity the reaction was sent to.
    pub target_entity: String,
    /// The action it asked for.
    pub target_action: String,
    /// The target's status when the reaction landed.
    pub target_state: String,
}

impl fmt::Display for DroppedReaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{} fired '{}' -> {}.{}, but {} was in '{}'",
            self.source_entity,
            self.source_action,
            self.trigger_name,
            self.target_entity,
            self.target_action,
            self.target_entity,
            self.target_state
        )
    }
}

/// What a drop check found.
pub(crate) struct Drops {
    /// Each distinct dropped reaction, in order.
    pub(crate) drops: Vec<DroppedReaction>,
    /// Distinct joint states walked.
    pub(crate) states: usize,
    /// Why the walk stopped before seeing every joint state.
    pub(crate) stopped: Option<String>,
}

impl JointModel {
    /// The joint model of every type in `scope`, whole (nothing sliced), one
    /// record of each that every reference finds, the dispatches a move
    /// sends landing before the next move from outside. Anyone sends
    /// anything, as ADR-0150's check assumed.
    pub(in crate::composite::joint) fn whole(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: BTreeSet<String>,
    ) -> Self {
        let (models, reactions) = tracked(by_name, &scope, BTreeMap::new(), Records::One);
        // The id fields references to types in scope use: in guards, in
        // reactions' guards, and in field-resolved reactions.
        let guards = models
            .iter()
            .flat_map(|(entity, model)| model.transitions.iter().map(move |t| (entity, &t.guard)))
            .chain(
                reactions
                    .iter()
                    .filter_map(|r| r.guard.as_ref().map(|guard| (&r.from, guard))),
            );
        let mut refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (entity, guard) in guards {
            for (related, id_field) in guard.related_types() {
                if scope.contains(&related) {
                    refs.entry(entity.clone()).or_default().insert(id_field);
                }
            }
        }
        for reaction in &reactions {
            if let Some(TargetResolver::Field { id_field }) = &reaction.resolve {
                refs.entry(reaction.from.clone())
                    .or_default()
                    .insert(id_field.clone());
            }
        }
        let mut model = Self::compose(
            by_name,
            &scope,
            (models, reactions),
            &BTreeMap::new(),
            Sending::anyone(),
            false,
        );
        model.records = Records::One;
        model.refs = refs;
        model.delivery = Delivery::BeforeNextRequest;
        model
    }

    /// Walk every joint state breadth first, in the memory `bound` gives
    /// the walk, naming each distinct reaction a target refuses when it
    /// lands. A move from outside waits until every dispatch in flight has
    /// landed, so the walk sees every order they can land in.
    pub(in crate::composite::joint) fn drops(self: &Arc<Self>, bound: Bound<'_>) -> Drops {
        let found = Arc::new(Mutex::new(BTreeSet::new()));
        let explored = {
            let found = Arc::clone(&found);
            self.walk_with(bound, Keep::States, move |model, _, state, _| {
                if !state.inflight.is_empty() {
                    let drops: Vec<_> = model
                        .moves(state)
                        .iter()
                        .filter_map(|mv| model.dropped(state, mv))
                        .collect();
                    if !drops.is_empty() {
                        found.lock().expect("a walk thread panicked").extend(drops);
                    }
                }
                false
            })
        };
        let drops = std::mem::take(&mut *found.lock().expect("a walk thread panicked"));
        Drops {
            drops: drops.into_iter().collect(),
            states: explored.len(),
            stopped: explored.stopped,
        }
    }

    /// The reaction `mv` drops: a dispatch a reaction sent that its target
    /// refuses, on the dispatch path or in its actor.
    fn dropped(&self, state: &JointState, mv: &Move) -> Option<DroppedReaction> {
        let (Move::Read(dispatch) | Move::Arrive(dispatch)) = mv else {
            return None;
        };
        let reaction = &self.reactions[dispatch.via?];
        // Reading its trigger's guard is no refusal.
        if reaction.may_drop || dispatch.unread_guard.is_some() {
            return None;
        }
        let refused = if dispatch.read {
            let mut rest = state.clone();
            remove(&mut rest, dispatch);
            matches!(self.arrive(&rest, dispatch), Arrival::Refused)
        } else {
            self.read_allows(dispatch, state) == Truth::False
        };
        (refused && !self.expansion.exhausted.get()).then(|| DroppedReaction {
            source_entity: reaction.from.clone(),
            source_action: reaction.source_action.clone(),
            trigger_name: reaction.trigger.clone(),
            target_entity: dispatch.to.to_string(),
            target_action: dispatch.action.to_string(),
            target_state: state.records[dispatch.to.as_str()].status.to_string(),
        })
    }
}

/// Walk the joint model of the types in `scope`, whole, one record of each,
/// in the memory `bound` gives it, and name every reaction a target refuses
/// when it lands (ADR-0150). The dispatches a move sends land, in any order,
/// before the next move from outside: the check claims no more than that.
pub(crate) fn dropped_reactions(
    automatons: &[&Automaton],
    scope: BTreeSet<String>,
    bound: Bound<'_>,
) -> Drops {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    Arc::new(JointModel::whole(&by_name, scope)).drops(bound)
}
