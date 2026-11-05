//! Who a reaction from a type outside the scope runs as: whoever may send
//! the move that set it off. The model holds no record of that move, so
//! this reads the whole app once: who may send each move as a request, and
//! how reactions, spawns, module answers and timers carry a sender on, as
//! [`Senders`] says the kernel does. A service the policies were not read
//! for, and a move nothing reads as sent, stay with the sender the model
//! does not know, which Cedar may answer either way for.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::May;
use temper_spec::automaton::{Action, ActionTrigger, Automaton, TargetResolver, TriggerKind};

use super::super::super::senders::{OUTSIDE, Senders, declared};
use super::super::read::Kernel;
use super::super::spawn::spawns_of;

/// Senders by move (entity, action).
type ByMove = BTreeMap<(String, String), BTreeSet<u32>>;

/// Whom a reaction from outside into one move may run as.
#[derive(Debug, Clone, Default)]
pub(super) struct SentAs {
    /// Every sender it may run as.
    pub(super) by: BTreeSet<u32>,
    /// Those that arrive as the start of a record a source spawns, which
    /// the kernel makes whatever Cedar says of the move.
    pub(super) spawned: BTreeSet<u32>,
}

/// The senders each reaction from a type outside `scope` into a move in it
/// may run as, by that move. `relies_on` names the moves outside code is
/// relied on to send, whoever sends them.
pub(super) fn sent_as(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    senders: &Senders,
    relies_on: &BTreeSet<(String, String)>,
) -> BTreeMap<(String, String), SentAs> {
    let all = by_name.keys().map(|name| name.to_string()).collect();
    let kernel = Kernel::read(by_name, &all);
    let mut sent = ByMove::new();
    for (entity, automaton) in by_name {
        for action in &automaton.actions {
            let key = (entity.to_string(), action.name.clone());
            let relied = relies_on.contains(&key);
            let may = senders.outside().iter().copied().filter(|&by| {
                relied || senders.request_anywhere(by, entity, &action.name) != May::No
            });
            sent.entry(key).or_default().extend(may);
        }
        // A record that starts in a state with a timer has it armed by no
        // one the model knows of: someone outside.
        for timeout in &kernel.timers[*entity] {
            if timeout.state == automaton.automaton.initial {
                let key = (entity.to_string(), timeout.action.clone());
                sent.entry(key).or_default().insert(senders.timer(OUTSIDE));
            }
        }
    }
    loop {
        let mut grew = false;
        for (entity, automaton) in by_name {
            for action in &automaton.actions {
                let key = (entity.to_string(), action.name.clone());
                let from: Vec<u32> = sent[&key].iter().copied().collect();
                for by in from {
                    for (to, next, _) in carried(automaton, entity, action, &kernel, senders, by) {
                        grew |= sent.entry(to).or_default().insert(next);
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }
    let unknown = [senders.unknown(), senders.timer(senders.unknown())];
    let mut into: BTreeMap<(String, String), SentAs> = BTreeMap::new();
    for (entity, automaton) in by_name.iter().filter(|(name, _)| !scope.contains(**name)) {
        for action in &automaton.actions {
            let from = &sent[&(entity.to_string(), action.name.clone())];
            let starts: Vec<u32> = if from.is_empty() {
                unknown.to_vec()
            } else {
                from.iter().copied().collect()
            };
            for by in starts {
                for (to, next, link) in carried(automaton, entity, action, &kernel, senders, by) {
                    if !scope.contains(&to.0) {
                        continue;
                    }
                    let sent = into.entry(to);
                    match link {
                        Link::Trigger(_) => sent.or_default().by.insert(next),
                        Link::Spawn => {
                            let sent = sent.or_default();
                            sent.spawned.insert(next);
                            sent.by.insert(next)
                        }
                        Link::Answer | Link::Timer => false,
                    };
                }
            }
        }
    }
    into
}

/// How a move sets off another.
#[derive(Clone, Copy)]
enum Link<'a> {
    /// A trigger's reaction.
    Trigger(&'a ActionTrigger),
    /// The start of a record the move spawns: the kernel's to make.
    Spawn,
    /// A module's answer to a call the move starts.
    Answer,
    /// A state timer the move arms or resets.
    Timer,
}

/// The moves `entity`'s `action` sets off, and how.
fn links<'a>(
    automaton: &'a Automaton,
    entity: &'a str,
    action: &'a Action,
    kernel: &'a Kernel,
) -> impl Iterator<Item = ((String, String), Link<'a>)> + 'a {
    let triggered = action.triggers.iter().filter_map(move |trigger| {
        let (TriggerKind::Entity, Some(to), Some(target)) = (
            &trigger.kind,
            &trigger.target_entity,
            &trigger.target_action,
        ) else {
            return None;
        };
        Some(((to.clone(), target.clone()), Link::Trigger(trigger)))
    });
    let spawned =
        spawns_of(action).map(move |(to, target, ..)| ((to.clone(), target.clone()), Link::Spawn));
    let calls = kernel
        .calls_on
        .get(&(entity.to_string(), action.name.clone()))
        .into_iter()
        .flatten();
    let answers = calls.flat_map(move |spec| {
        let named: Vec<&String> = spec.on_success.iter().chain(&spec.on_failure).collect();
        let names: Vec<&String> = if spec.on_success.is_empty() {
            automaton
                .actions
                .iter()
                .map(|a| &a.name)
                .chain(named)
                .collect()
        } else {
            named
        };
        names
            .into_iter()
            .map(move |name| ((entity.to_string(), name.clone()), Link::Answer))
    });
    let timers = kernel.timers.get(entity).into_iter().flatten();
    let fired = timers
        .filter(move |timeout| {
            action.to.as_deref() == Some(timeout.state.as_str())
                || timeout.reset_on.contains(&action.name)
        })
        .map(move |timeout| ((entity.to_string(), timeout.action.clone()), Link::Timer));
    triggered.chain(spawned).chain(answers).chain(fired)
}

/// The moves `entity`'s `action`, sent by `by`, sets off, with whom each
/// runs as, and how it gets there.
fn carried<'a>(
    automaton: &'a Automaton,
    entity: &'a str,
    action: &'a Action,
    kernel: &'a Kernel,
    senders: &'a Senders,
    by: u32,
) -> impl Iterator<Item = ((String, String), u32, Link<'a>)> + 'a {
    links(automaton, entity, action, kernel).map(move |(to, link)| {
        let next = match link {
            Link::Trigger(trigger) => match declared(trigger, entity, &action.name) {
                Some(service) => senders.declared_as(&service, by),
                None => {
                    let same_id = matches!(trigger.resolve_target, Some(TargetResolver::SameId));
                    senders.reaction(None, by, same_id)
                }
            },
            Link::Spawn => senders.reaction(None, by, false),
            Link::Answer => by,
            Link::Timer => senders.timer(Senders::armer(by)),
        };
        (to, next, link)
    })
}

/// What can set off each move (entity, action) of the app, through any
/// link: a reaction, a spawn, a module's answer or a timer.
pub(super) fn set_off_by(
    by_name: &BTreeMap<&str, &Automaton>,
) -> BTreeMap<(String, String), BTreeSet<(String, String)>> {
    let all = by_name.keys().map(|name| name.to_string()).collect();
    let kernel = Kernel::read(by_name, &all);
    let mut earlier: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    for (entity, automaton) in by_name {
        for action in &automaton.actions {
            for (to, _) in links(automaton, entity, action, &kernel) {
                earlier
                    .entry(to)
                    .or_default()
                    .insert((entity.to_string(), action.name.clone()));
            }
        }
    }
    earlier
}
