//! What the checker reads out of the spec.
//!
//! One place decides how a spec's actions are read: which source states an
//! action may fire from, which states end the run, and which actions are
//! emissions rather than transitions. [`walk`](super::walk) asks; it does not
//! interpret the spec itself.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Action, Automaton, Guard};

use super::{ViolationKind, KERNEL_PLATFORM_ACTIONS};

/// The source states an action may fire from.
pub(super) enum SourceStates<'a> {
    /// Exactly these states.
    Declared(BTreeSet<&'a str>),
    /// Every state in the automaton.
    AnyState,
    /// Nothing this checker can evaluate.
    Unevaluable,
}

/// Whether the action never transitions the entity.
///
/// `kind = "output"` actions are emissions to the environment; the kernel
/// leaves them out of the transition table entirely
/// (`TransitionTable::from_automaton`), so they neither fire from a
/// state nor land in one.
fn is_emitted_event(action: &Action) -> bool {
    action.kind == "output"
}

/// Legal source states for an action.
///
/// Mirrors the kernel's own reading of a spec
/// (`Automaton` / `TransitionTable::from_automaton`): a `from` list is the source
/// set; an action written with a `state_in` guard instead still restricts its
/// sources; and an `input` or `Composite` action with neither is always
/// enabled, which is the I/O-automata property the kernel implements by giving
/// it every state as a source.
fn legal_sources(action: &Action) -> SourceStates<'_> {
    if !action.from.is_empty() {
        return SourceStates::Declared(action.from.iter().map(String::as_str).collect());
    }
    // An action written with a `state_in` guard instead of a `from` list still
    // restricts its source states; read the guard rather than treating the
    // action as unconstrained.
    let guarded: BTreeSet<&str> = action
        .guard
        .iter()
        .filter_map(|guard| match guard {
            Guard::StateIn { values } => Some(values.iter().map(String::as_str)),
            _ => None,
        })
        .flatten()
        .collect();
    if !guarded.is_empty() {
        return SourceStates::Declared(guarded);
    }
    if is_emitted_event(action) {
        return SourceStates::Unevaluable;
    }
    if action.kind == "input" || action.kind.eq_ignore_ascii_case("composite") {
        return SourceStates::AnyState;
    }
    // An `internal` action with no source at all fires from nowhere in the
    // kernel's transition table. That is a spec-authoring fault rather than a
    // run fault, so the row is reported as unchecked instead of condemned.
    SourceStates::Unevaluable
}

/// States no action can fire from.
///
/// This is the kernel's own terminal-state rule — a state no action lists as a
/// source — extended to read `state_in` guards as well as `from` lists, and to
/// honour the always-enabled property of unconstrained input actions.
/// `Automaton::extract_metadata` reads only `from`, so an action written with
/// a guard instead would leave its own source states looking terminal and turn
/// every legal action out of them into a false `post_terminal`. For the same
/// reason a single always-enabled action empties the terminal set: it may fire
/// from every state, so no state ends the run.
fn terminal_states(automaton: &Automaton) -> BTreeSet<String> {
    let mut sources: BTreeSet<&str> = BTreeSet::new();
    for action in &automaton.actions {
        match legal_sources(action) {
            SourceStates::Declared(action_sources) => sources.extend(action_sources),
            SourceStates::AnyState => return BTreeSet::new(),
            SourceStates::Unevaluable => {}
        }
    }
    automaton
        .automaton
        .states
        .iter()
        .filter(|state| !sources.contains(state.as_str()))
        .cloned()
        .collect()
}

/// The parts of the automaton the checker consults, resolved once.
pub(super) struct SpecView<'a> {
    pub(super) entity_name: &'a str,
    pub(super) declared: BTreeMap<&'a str, &'a Action>,
    pub(super) terminal_states: BTreeSet<String>,
}

impl<'a> SpecView<'a> {
    pub(super) fn new(automaton: &'a Automaton) -> Self {
        Self {
            entity_name: automaton.automaton.name.as_str(),
            declared: automaton
                .actions
                .iter()
                .map(|action| (action.name.as_str(), action))
                .collect(),
            terminal_states: terminal_states(automaton),
        }
    }

    /// Legal source states for an action.
    pub(super) fn legal_sources(&self, action: &'a Action) -> SourceStates<'a> {
        legal_sources(action)
    }

    pub(super) fn is_emitted_event(&self, action: &Action) -> bool {
        is_emitted_event(action)
    }

    /// Classify an action name this actor's spec does not declare.
    pub(super) fn classify_undeclared(&self, action: &str) -> ViolationKind {
        if KERNEL_PLATFORM_ACTIONS.contains(&action) {
            ViolationKind::ForbiddenAction
        } else {
            ViolationKind::UnknownAction
        }
    }
}
