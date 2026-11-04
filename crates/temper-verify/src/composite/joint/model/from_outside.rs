//! Reactions from types outside a joint model's scope into types in it.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::Automaton;
use temper_spec::predicate::Truth;

use super::super::JointState;
use super::super::state::ReactionStep;
use super::JointModel;
use super::commit::add;
use super::spawn::spawns_of;
use super::step::{Certainty, Move, sent};
use crate::model::semantics::successors_budgeted;
use crate::model::{ModelString, ResolvedTransition, TemperModelAction};

/// Reactions from types outside the scope into one action of a type in it.
///
/// The model holds no record of their source, so it cannot tell when one
/// fires. It sends such a reaction as a move from outside: whenever its
/// target's guard may let it, with any parameters, as a sender Cedar may
/// answer either way for ([`super::senders::Senders::unknown`]). That only
/// adds behaviour, so an invariant that holds here holds in the runtime.
/// Liveness never counts on one (`liveness.rs`).
#[derive(Debug, Clone)]
pub(super) struct Outside {
    /// The types it comes from.
    from: BTreeSet<String>,
    /// The steps (entity, action) its chain may already hold when it
    /// arrives: its sources' moves, and every move that can set one of those
    /// off through the app's reactions. A reaction it sets off that repeats
    /// one of them may be stopped by the runtime or not.
    before: BTreeSet<(String, String)>,
    /// Whether the record it reaches is one its chain made, as its step
    /// records it: `None` when no source makes it, the target type when
    /// every source does, and [`MAYBE_MADE`] otherwise.
    made: Option<String>,
}

/// A step's `generated_for` when the model cannot tell whether its chain
/// made its record: a step repeats it only maybe, unless both are unsure.
pub(super) const MAYBE_MADE: &str = "?";

impl JointModel {
    /// The types outside the scope whose reactions send a move here.
    pub(in crate::composite::joint) fn outside_types(&self) -> BTreeSet<String> {
        self.outside
            .values()
            .flat_map(|outside| outside.from.iter().cloned())
            .collect()
    }

    /// The types outside the scope whose reactions send `entity`'s
    /// `action`, when any do.
    pub(in crate::composite::joint) fn outside_from(
        &self,
        entity: &str,
        action: &str,
    ) -> Option<&BTreeSet<String>> {
        self.outside
            .get(&(entity.to_string(), action.to_string()))
            .map(|outside| &outside.from)
    }

    /// The chain a reaction from outside the scope into `entity`'s `action`
    /// arrives with: a mark standing for the steps it may already hold
    /// ([`Outside::before`]), then its own step.
    pub(super) fn outside_chain(&self, entity: &str, action: &str) -> Vec<ReactionStep> {
        let made = self
            .outside
            .get(&(entity.to_string(), action.to_string()))
            .and_then(|outside| outside.made.clone());
        vec![
            ReactionStep {
                entity_type: String::new(),
                action: format!("{entity}.{action}"),
                generated_for: None,
            },
            ReactionStep {
                entity_type: entity.to_string(),
                action: action.to_string(),
                generated_for: made,
            },
        ]
    }

    /// Whether the runtime stops `step` because its chain `chain` already
    /// holds it: true, false, or unknown when a step from outside the scope
    /// may hold it, or one of the two may have made its record and the
    /// other not.
    pub(super) fn repeats(&self, chain: &[ReactionStep], step: &ReactionStep) -> Truth {
        let mut maybe = false;
        for taken in chain {
            if taken.entity_type.is_empty() {
                let key = taken
                    .action
                    .split_once('.')
                    .map(|(e, a)| (e.to_string(), a.to_string()));
                maybe |= key
                    .and_then(|key| self.outside.get(&key))
                    .is_some_and(|outside| {
                        outside
                            .before
                            .contains(&(step.entity_type.clone(), step.action.clone()))
                    });
                continue;
            }
            if taken.entity_type != step.entity_type || taken.action != step.action {
                continue;
            }
            // A chain has one outside origin, and `same_id` carries its
            // origin on, so two unsure steps stand for the same one.
            let unsure = |s: &ReactionStep| s.generated_for.as_deref() == Some(MAYBE_MADE);
            if unsure(taken) != unsure(step) {
                maybe = true;
            } else if step.repeats(taken) {
                return Truth::True;
            }
        }
        if maybe { Truth::Unknown } else { Truth::False }
    }

    /// The moves a reaction from a type outside the scope into `t` makes
    /// from `state`: with any parameters, in either chain a sender may be
    /// in, as a sender the model does not know. False when the expansion's
    /// room is used up.
    pub(super) fn outside_moves(
        &self,
        entity: &str,
        t: &ResolvedTransition,
        state: &JointState,
        values: &BTreeSet<ModelString>,
        out: &mut Vec<Move>,
    ) -> bool {
        let unknown = self.senders.unknown();
        let mut senders = vec![unknown];
        if self.senders.timer(unknown) != unknown {
            senders.push(self.senders.timer(unknown));
        }
        let local = &state.records[entity];
        for (params, _) in
            successors_budgeted(&self.models[entity], t, local, values, &self.expansion)
        {
            for &by in &senders {
                let action = TemperModelAction {
                    name: t.name.clone(),
                    target_state: t.to_state.clone(),
                    params: params.clone(),
                };
                if !self.keep_move(out, Move::Reaction(entity.to_string(), action, by)) {
                    return false;
                }
            }
        }
        true
    }

    /// The states a reaction from outside the scope into `entity`'s
    /// `action`, as `by`, leads to from `state`. Nothing the model holds
    /// decides whether it is sent or what Cedar says of it, so it may also
    /// not happen.
    pub(super) fn outside_reaction(
        &self,
        state: &JointState,
        entity: &str,
        action: &TemperModelAction,
        by: u32,
    ) -> Vec<(JointState, String, Certainty)> {
        let from = self
            .outside_from(entity, &action.name)
            .map_or_else(String::new, |from| {
                from.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" or ")
            });
        let text = format!("{entity}.{action} (a reaction from {from})");
        let chain = self.outside_chain(entity, &action.name);
        if self.action_reads_related(entity, &action.name) {
            let mut next = state.clone();
            let mut dispatch = sent(entity, action, by);
            dispatch.chain = chain;
            add(&mut next, dispatch);
            return vec![(next, format!("{text} sent"), Certainty::Unsure)];
        }
        self.commit(state.clone(), entity, action, &chain, by)
            .into_iter()
            .map(|(outcome, then)| (outcome, text.clone(), Certainty::Unsure.then(then)))
            .collect()
    }
}

/// How a spawn reaches its child: it makes it.
static CREATE: Option<temper_spec::automaton::TargetResolver> =
    Some(temper_spec::automaton::TargetResolver::Create);

/// All a model of `scope` reads of the types outside it, as text.
pub(in crate::composite::joint) fn reads_beyond(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
) -> String {
    format!(
        "{:?}{:?}",
        outside_reactions(by_name, scope),
        super::read::reactions_into(by_name, scope)
    )
}

/// The reactions from types outside `scope` into actions of types in it
/// ([`Outside`]), by their target (entity, action).
pub(super) fn outside_reactions(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
) -> BTreeMap<(String, String), Outside> {
    use temper_spec::automaton::{TargetResolver, TriggerKind};
    // Each move (entity, action) of the app, with the moves its reactions
    // send, read backwards: what can set off each move.
    let mut set_off_by: BTreeMap<(String, String), BTreeSet<(String, String)>> = BTreeMap::new();
    for (entity, automaton) in by_name {
        for action in &automaton.actions {
            for (to, target, ..) in spawns_of(action) {
                set_off_by
                    .entry((to.clone(), target.clone()))
                    .or_default()
                    .insert((entity.to_string(), action.name.clone()));
            }
            for trigger in &action.triggers {
                if let (TriggerKind::Entity, Some(to), Some(target)) = (
                    &trigger.kind,
                    &trigger.target_entity,
                    &trigger.target_action,
                ) {
                    set_off_by
                        .entry((to.clone(), target.clone()))
                        .or_default()
                        .insert((entity.to_string(), action.name.clone()));
                }
            }
        }
    }
    let mut outside: BTreeMap<(String, String), Outside> = BTreeMap::new();
    let mut makes: BTreeMap<(String, String), BTreeSet<Option<bool>>> = BTreeMap::new();
    for (from, automaton) in by_name.iter().filter(|(name, _)| !scope.contains(**name)) {
        for action in &automaton.actions {
            let spawned = spawns_of(action).map(|(to, target, ..)| (to, target, &CREATE));
            let triggered = action.triggers.iter().filter_map(|trigger| {
                let (TriggerKind::Entity, Some(to), Some(target)) = (
                    &trigger.kind,
                    &trigger.target_entity,
                    &trigger.target_action,
                ) else {
                    return None;
                };
                Some((to, target, &trigger.resolve_target))
            });
            for (to, target, resolve) in spawned.chain(triggered) {
                if !scope.contains(to) {
                    continue;
                }
                let key = (to.clone(), target.clone());
                let entry = outside.entry(key.clone()).or_insert_with(|| Outside {
                    from: BTreeSet::new(),
                    before: BTreeSet::new(),
                    made: None,
                });
                entry.from.insert(from.to_string());
                // Everything that can come before its source's move, and
                // that move.
                let mut queue = vec![(from.to_string(), action.name.clone())];
                while let Some(step) = queue.pop() {
                    if let Some(earlier) = set_off_by.get(&step) {
                        queue.extend(
                            earlier
                                .iter()
                                .filter(|e| !entry.before.contains(*e))
                                .cloned(),
                        );
                    }
                    entry.before.insert(step);
                }
                // As `react` records its step: made when it creates its
                // target; a same-id step inherits what its chain made.
                makes.entry(key).or_default().insert(match resolve {
                    Some(TargetResolver::Create) => Some(true),
                    Some(TargetResolver::SameId | TargetResolver::CreateIfMissing { .. }) => None,
                    _ => Some(false),
                });
            }
        }
    }
    for (key, ways) in makes {
        if let Some(entry) = outside.get_mut(&key) {
            entry.made = match ways.into_iter().collect::<Vec<_>>().as_slice() {
                [Some(false)] => None,
                [Some(true)] => Some(key.0.clone()),
                _ => Some(MAYBE_MADE.to_string()),
            };
        }
    }
    outside
}
