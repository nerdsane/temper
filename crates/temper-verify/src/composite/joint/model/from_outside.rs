//! Reactions from types outside a joint model's scope into types in it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use temper_authz::May;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::Truth;

use super::super::JointState;
use super::super::senders::Senders;
use super::super::state::ReactionStep;
use super::JointModel;
use super::commit::add;
use super::spawn::spawns_of;
use super::step::{Certainty, Move, sent};
use crate::model::semantics::successors_budgeted;
use crate::model::{ModelString, ResolvedTransition, TemperModelAction};

mod sent_as;

/// Reactions from types outside the scope into one action of a type in it.
///
/// The model holds no record of their source, so it cannot tell when one
/// fires. It sends such a reaction as a move from outside: whenever its
/// target's guard may let it, with any parameters, as each sender that may
/// send the move that set it off ([`sent_as`]), or, where the model cannot
/// tell, as a sender Cedar may answer either way for
/// ([`Senders::unknown`]). That only adds behaviour, so an invariant that
/// holds here holds in the runtime. Liveness never counts on one
/// (`liveness.rs`).
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
    /// Whether a source spawns it: the kernel starts a spawned record, and
    /// Cedar never decides that first move. Where the senders were read,
    /// only those that arrive by a spawn skip Cedar ([`sent_as::SentAs`]).
    spawned: bool,
    /// The senders it may run as, when they were read.
    by: Option<sent_as::SentAs>,
    /// Of those, one for each way what follows can go, once worked out.
    kept: Kept,
}

/// The senders a reaction from outside is sent as, worked out on first
/// use: what a walk learns, not what the model is.
#[derive(Clone, Default)]
struct Kept(OnceLock<Arc<[u32]>>);

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kept")
    }
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
            .get(pair!(entity, action))
            .map(|outside| &outside.from)
    }

    /// The chain a reaction from outside the scope into `entity`'s `action`
    /// arrives with: a mark standing for the steps it may already hold
    /// ([`Outside::before`]), then its own step.
    pub(super) fn outside_chain(&self, entity: &str, action: &str) -> Vec<ReactionStep> {
        let made = self
            .outside
            .get(pair!(entity, action))
            .and_then(|outside| outside.made.clone());
        vec![
            ReactionStep {
                entity_type: "".into(),
                action: format!("{entity}.{action}").into(),
                generated_for: None,
            },
            ReactionStep {
                entity_type: entity.into(),
                action: action.into(),
                generated_for: made.map(Into::into),
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
                maybe |= taken
                    .action
                    .split_once('.')
                    .and_then(|(e, a)| self.outside.get(pair!(e, a)))
                    .is_some_and(|outside| {
                        outside
                            .before
                            .contains(pair!(&step.entity_type, &step.action))
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
        let senders = self.outside_sent_as(entity, &t.name);
        let local = &state.records[entity];
        for (params, _) in
            successors_budgeted(&self.models[entity], t, local, values, &self.expansion)
        {
            for &by in senders.iter() {
                let action = TemperModelAction {
                    name: t.name,
                    target_state: t.to_state,
                    params: params.clone(),
                };
                if !self.keep_move(out, Move::Reaction(entity.to_string(), action, by)) {
                    return false;
                }
            }
        }
        true
    }

    /// The senders a reaction from outside the scope into `entity`'s
    /// `action` is sent as: of those that may send what set it off and that
    /// Cedar may let send it, one for each way what follows can go; the sender the model does not know,
    /// in either chain, where those were not read.
    fn outside_sent_as(&self, entity: &str, action: &str) -> Arc<[u32]> {
        let unknown = || {
            let unknown = self.senders.unknown();
            BTreeSet::from([unknown, self.senders.timer(unknown)])
        };
        let Some(outside) = self.outside.get(&(entity.to_string(), action.to_string())) else {
            return unknown().into_iter().collect();
        };
        let kept = outside.kept.0.get_or_init(|| {
            let (by, spawned) = match &outside.by {
                Some(sent) => (sent.by.clone(), sent.spawned.clone()),
                None if outside.spawned => (unknown(), unknown()),
                None => (unknown(), BTreeSet::new()),
            };
            let asked = self.asked_after(entity, action);
            let mut ways = BTreeSet::new();
            // Cedar decides the reaction as its sender: one it surely refuses
            // never arrives. The kernel's start of a spawned record is not
            // Cedar's to decide.
            by.into_iter()
                .filter(|&by| {
                    let may = if spawned.contains(&by) {
                        May::Yes
                    } else {
                        self.senders.may(by, entity, action)
                    };
                    may != May::No && ways.insert(self.way(by, may, &asked))
                })
                .collect()
        });
        kept.clone()
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
        let as_whom = || {
            if self.outside_sent_as(entity, &action.name).len() > 1 {
                format!(", as {}", self.senders.name(by))
            } else {
                String::new()
            }
        };
        let text = words!(
            self,
            "{entity}.{action} (a reaction from {from}{})",
            as_whom()
        );
        let chain = self.outside_chain(entity, &action.name);
        if self.action_reads_related(entity, &action.name) {
            let mut next = state.clone();
            let mut dispatch = sent(entity, action, by, None);
            dispatch.chain = chain.into();
            add(&mut next, dispatch);
            return vec![(next, words!(self, "{text} sent"), Certainty::Unsure)];
        }
        self.commit(state.clone(), entity, action, &chain, by, None)
            .into_iter()
            .map(|(outcome, then)| (outcome, text.clone(), Certainty::Unsure.then(then)))
            .collect()
    }
}

/// How a spawn reaches its child: it makes it.
static CREATE: Option<temper_spec::automaton::TargetResolver> =
    Some(temper_spec::automaton::TargetResolver::Create);

/// The moves outside `scope` that can set off a reaction into one of
/// `matters`, the types in it whose reactions run as their sender: whoever
/// may send such a move starts a chain that reaches them, so a kind of
/// outside sender that may send one is kept.
pub(in crate::composite::joint) fn starts(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    matters: &BTreeSet<String>,
) -> BTreeSet<(String, String)> {
    // A chain may pass through module answers and timers on its way, which
    // carry its sender on.
    let earlier = sent_as::set_off_by(by_name);
    let mut queue: Vec<(String, String)> = outside_reactions(by_name, scope, None)
        .into_iter()
        .filter(|((entity, _), _)| matters.contains(entity))
        .flat_map(|(_, outside)| outside.before)
        .collect();
    let mut found = BTreeSet::new();
    while let Some(step) = queue.pop() {
        if let Some(before) = earlier.get(&step) {
            queue.extend(before.iter().filter(|e| !found.contains(*e)).cloned());
        }
        found.insert(step);
    }
    found
        .into_iter()
        .filter(|(from, _)| !scope.contains(from))
        .collect()
}

/// All a model of `scope` reads of the types outside it, as text.
pub(in crate::composite::joint) fn reads_beyond(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
) -> String {
    format!(
        "{:?}{:?}",
        outside_reactions(by_name, scope, None),
        super::read::reactions_into(by_name, scope)
    )
}

/// The reactions from types outside `scope` into actions of types in it
/// ([`Outside`]), by their target (entity, action).
/// With `sent` (the senders and the moves outside code is relied on to
/// send), each also knows whom it may run as.
pub(super) fn outside_reactions(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    sent: Option<(&Senders, &BTreeSet<(String, String)>)>,
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
            let spawned = spawns_of(action).map(|(to, target, ..)| (to, target, &CREATE, true));
            let triggered = action.triggers.iter().filter_map(|trigger| {
                let (TriggerKind::Entity, Some(to), Some(target)) = (
                    &trigger.kind,
                    &trigger.target_entity,
                    &trigger.target_action,
                ) else {
                    return None;
                };
                Some((to, target, &trigger.resolve_target, false))
            });
            for (to, target, resolve, spawn) in spawned.chain(triggered) {
                if !scope.contains(to) {
                    continue;
                }
                let key = (to.clone(), target.clone());
                let entry = outside.entry(key.clone()).or_insert_with(|| Outside {
                    from: BTreeSet::new(),
                    before: BTreeSet::new(),
                    made: None,
                    spawned: false,
                    by: None,
                    kept: Kept::default(),
                });
                entry.from.insert(from.to_string());
                entry.spawned |= spawn;
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
    if let Some((senders, relies_on)) = sent {
        for (key, by) in sent_as::sent_as(by_name, scope, senders, relies_on) {
            if let Some(entry) = outside.get_mut(&key) {
                entry.by = Some(by);
            }
        }
    }
    outside
}
